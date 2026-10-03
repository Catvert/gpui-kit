//! One evaluation of an application, mounted in many places and called with
//! no view at all.
//!
//! [`ShellRuntime::load_application`] gives an embedder one view per
//! evaluation: the entry's default export, mounted once, the generation
//! released with it. An embedder that shows one application in several places
//! — a list in one pane and the item it opens in another, and code that runs
//! when nothing of it is on screen — wants them to share that evaluation, so
//! that what one of them learns the others see.
//!
//! An [`ApplicationInstance`] is that evaluation. [`ApplicationInstance::mount`]
//! builds a view from a **named export**, as a nested view is built: owned by a
//! handle of its own, its tasks and retained records keyed to it, so releasing
//! one leaves its siblings and the module's state alone.
//! [`ApplicationInstance::call`] runs an exported function under the same
//! policy and generation, with no view. The generation — and every view still
//! mounted from it — goes when the instance is unloaded or dropped.
//!
//! The policy is the default in force at the load, as for `load_application`:
//! that is the one the entry's imports were linked against, so it is the only
//! one under which they mean anything.

use std::{cell::RefCell, collections::HashMap, path::Path, rc::Rc};

use anyhow::{Context as _, Result};
use gpui::{App, Entity, EntityId, Window};
use rquickjs::{Exception, Object, Persistent, Value, function::Args as JsArgs};

use super::{
    ApplicationModuleLease, ContextBinding, ShellRuntime, ViewType, context_object, host_modules,
    scheduler,
};
use crate::{
    entities::EntityHandle,
    host_modules::HostValue,
    policy::Policy,
    runtime::ApplicationGeneration,
    scope::{self, ScopePhase},
    view::ScriptView,
};

/// An application evaluated once, whose exports are mounted and called.
pub struct ApplicationInstance {
    /// The entry's namespace. Declared before `runtime`, and behind an
    /// `Option` so `Drop` can release it first: a script value released after
    /// its engine aborts the process.
    namespace: Option<Persistent<Object<'static>>>,
    module_lease: Option<ApplicationModuleLease>,
    application: Rc<ApplicationGeneration>,
    policy: Rc<Policy>,
    /// The handle each mounted view was built under, by the view.
    mounted: RefCell<HashMap<EntityId, EntityHandle>>,
    runtime: Rc<ShellRuntime>,
}

impl std::fmt::Debug for ApplicationInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApplicationInstance")
            .field("mounted", &self.mounted.borrow().len())
            .finish_non_exhaustive()
    }
}

impl ShellRuntime {
    /// Evaluates an application's entry once, for [`ApplicationInstance::mount`]
    /// and [`ApplicationInstance::call`] to use as often as they like.
    ///
    /// Unlike [`Self::load_application`], the entry need not have a default
    /// export: what is mounted or called is named.
    pub fn load_instance(
        self: &Rc<Self>,
        directory: &Path,
        entry: &str,
    ) -> Result<ApplicationInstance> {
        let policy = crate::policy::default();
        let runtime = self.clone();
        self.evaluate_app(
            directory,
            entry,
            move |ctx, namespace, lease, application| {
                let Some(application) = application else {
                    return Err(Exception::throw_message(
                        ctx,
                        "an evaluated application has no generation",
                    ));
                };
                Ok(ApplicationInstance {
                    namespace: Some(Persistent::save(ctx, namespace)),
                    module_lease: lease,
                    application,
                    policy,
                    mounted: RefCell::new(HashMap::new()),
                    runtime,
                })
            },
        )
    }
}

impl ApplicationInstance {
    /// The names the entry exports, `default` among them if it has one.
    pub fn exports(&self) -> Result<Vec<String>> {
        let namespace = self.namespace()?;
        self.runtime.with_js(|ctx| {
            namespace
                .restore(ctx)?
                .keys::<String>()
                .collect::<rquickjs::Result<Vec<_>>>()
        })
    }

    /// Builds a view of the class exported as `export`, its `init` handed
    /// `props`.
    ///
    /// Repeatable: every view shares the module's state. A view that fails to
    /// build leaves nothing behind, and the instance as it was.
    pub fn mount(
        &self,
        export: &str,
        props: Option<HostValue>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<ScriptView>> {
        self.ensure_active()?;
        let namespace = self.namespace()?;
        let (class, props) = self.runtime.with_js(|ctx| {
            let class: Value = namespace.restore(ctx)?.get(export)?;
            let Some(class) = class.as_object() else {
                return Err(Exception::throw_type(
                    ctx,
                    &format!("the application exports no view class named `{export}`"),
                ));
            };
            let props = match props {
                Some(props) => Some(Persistent::save(ctx, host_modules::into_js(ctx, props)?)),
                None => None,
            };
            Ok((Persistent::save(ctx, class.clone()), props))
        })?;
        let view_type = ViewType {
            value: class,
            module_lease: self.module_lease.clone(),
            application: Some(self.application.clone()),
        };
        let handle = self.runtime.instantiate_nested_view(
            &view_type,
            self.policy.clone(),
            props,
            window,
            cx,
        )?;
        let view = self
            .runtime
            .entities()
            .view(handle)
            .context("a view just built is not in the store")?;
        self.mounted.borrow_mut().insert(view.entity_id(), handle);
        Ok(view)
    }

    /// Releases a view [`Self::mount`] built: its tasks and what it retained.
    /// The module and the other views are untouched.
    ///
    /// `false` for a view this instance did not mount, or already released.
    pub fn release(&self, view: &Entity<ScriptView>, cx: &mut App) -> bool {
        let handle = self.mounted.borrow_mut().remove(&view.entity_id());
        handle.is_some_and(|handle| self.runtime.release_view_handle(handle, cx))
    }

    /// Calls the function exported as `export` with `arguments`, then `cx`,
    /// the way a component callback is called; what it returns is dropped.
    ///
    /// It runs with no view: what it starts belongs to the application, and
    /// lives until the instance is unloaded. The promise jobs it queues are
    /// drained before this returns, as after any event.
    pub fn call(
        &self,
        export: &str,
        arguments: Vec<HostValue>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<()> {
        self.ensure_active()?;
        let namespace = self.namespace()?;
        let (guard, generation) = scope::enter_with_application(
            &self.runtime,
            window,
            cx,
            ScopePhase::Event,
            None,
            self.policy.clone(),
            Some(self.application.clone()),
        );
        let result = self.runtime.with_js(|ctx| {
            let function: Value = namespace.restore(ctx)?.get(export)?;
            let Some(function) = function.as_function().cloned() else {
                return Err(Exception::throw_type(
                    ctx,
                    &format!("the application exports no function named `{export}`"),
                ));
            };
            let mut js_arguments = JsArgs::new(ctx.clone(), arguments.len() + 1);
            for argument in arguments {
                js_arguments.push_arg(host_modules::into_js(ctx, argument)?)?;
            }
            js_arguments.push_arg(context_object(ctx, ContextBinding::Call(generation))?)?;
            function.call_arg::<Value>(js_arguments).map(drop)
        });
        scheduler::drain_runtime_jobs(&self.runtime, window, cx);
        drop(guard);
        result
    }

    /// Whether the generation still runs: not unloaded, nor retired by its
    /// runtime.
    pub fn is_active(&self) -> bool {
        self.application.is_active()
    }

    /// Releases the generation now, with every view still mounted from it —
    /// what dropping the instance does, but with the `App` that the cleanup of
    /// the application's effects needs.
    pub fn unload(self, cx: &mut App) {
        if self.application.is_active() {
            self.mounted.borrow_mut().clear();
            self.runtime
                .release_application_generation(&self.application, cx);
        }
    }

    fn ensure_active(&self) -> Result<()> {
        anyhow::ensure!(
            self.application.is_active(),
            "this application has been unloaded"
        );
        Ok(())
    }

    fn namespace(&self) -> Result<Persistent<Object<'static>>> {
        self.namespace
            .clone()
            .context("this application has been unloaded")
    }
}

impl Drop for ApplicationInstance {
    fn drop(&mut self) {
        if self.application.is_active() {
            self.mounted.borrow_mut().clear();
            self.runtime
                .release_application_generation_without_context(&self.application);
        }
        self.namespace.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Empty, TestAppContext, VisualTestContext};

    /// An application folder holding `main.js`, removed when dropped.
    struct Folder(std::path::PathBuf);

    impl Folder {
        fn new(name: &str, source: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "gpui-shell-instance-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("application directory");
            std::fs::write(root.join("main.js"), source).expect("application source");
            Self(root)
        }
    }

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Two view classes and two functions over one module-level record:
    /// whatever one of them does, the others see.
    const SHARED: &str = r#"
import { View } from "gpui-kit";
const shared = { inits: [], count: 0 };
export class List extends View {
  init(props) { shared.inits.push(props?.label ?? "-"); }
  render(cx) { return "list"; }
}
export class Detail extends View {
  init() { shared.inits.push("detail"); }
  render(cx) { return "detail"; }
}
export function bump(amount, cx) { shared.count += amount; }
export function probe(cx) {
  globalThis.__probe = `${shared.count} ${shared.inits.join(",")}`;
}
export const notAFunction = 3;
"#;

    fn probe(runtime: &Rc<ShellRuntime>) -> String {
        runtime
            .with_js(|ctx| ctx.globals().get::<_, String>("__probe"))
            .expect("probe")
    }

    #[gpui::test]
    fn views_and_calls_share_one_evaluation(cx: &mut TestAppContext) {
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        let app = Folder::new("shared", SHARED);
        let instance = runtime.load_instance(&app.0, "main.js").expect("instance");
        let window = cx.add_window(|_, _| Empty);
        let mut context = VisualTestContext::from_window(*window, cx);

        context.update(|window, cx| {
            let props = HostValue::Object(vec![("label".into(), HostValue::Str("a".into()))]);
            instance
                .mount("List", Some(props), window, cx)
                .expect("list");
            instance.mount("Detail", None, window, cx).expect("detail");
            instance
                .mount("List", None, window, cx)
                .expect("a second list");
            instance
                .call("bump", vec![HostValue::Number(10.)], window, cx)
                .expect("bump");
            instance
                .call("bump", vec![HostValue::Number(2.)], window, cx)
                .expect("bump");
            instance.call("probe", vec![], window, cx).expect("probe");
        });

        assert_eq!(probe(&runtime), "12 a,detail,-");
        context.update(|_, cx| instance.unload(cx));
    }

    #[gpui::test]
    fn releasing_one_view_leaves_its_siblings_and_the_module(cx: &mut TestAppContext) {
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        let app = Folder::new("release", SHARED);
        let instance = runtime.load_instance(&app.0, "main.js").expect("instance");
        let window = cx.add_window(|_, _| Empty);
        let mut context = VisualTestContext::from_window(*window, cx);

        let (first, second) = context.update(|window, cx| {
            (
                instance.mount("List", None, window, cx).expect("first"),
                instance.mount("Detail", None, window, cx).expect("second"),
            )
        });
        let handle_of = |view: &Entity<ScriptView>| instance.mounted.borrow()[&view.entity_id()];
        let (first_handle, second_handle) = (handle_of(&first), handle_of(&second));

        assert!(context.update(|_, cx| instance.release(&first, cx)));
        assert!(
            !context.update(|_, cx| instance.release(&first, cx)),
            "released twice"
        );
        assert!(runtime.entities().view(first_handle).is_none());
        assert!(runtime.entities().view(second_handle).is_some());
        assert!(instance.is_active());

        context.update(|window, cx| {
            instance
                .call("bump", vec![HostValue::Number(1.)], window, cx)
                .expect("bump");
            instance.call("probe", vec![], window, cx).expect("probe");
        });
        assert_eq!(probe(&runtime), "1 -,detail");

        context.update(|_, cx| instance.unload(cx));
        assert!(
            runtime.entities().view(second_handle).is_none(),
            "unloading releases what is still mounted"
        );
    }

    #[gpui::test]
    fn dropping_the_instance_releases_the_generation(cx: &mut TestAppContext) {
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        let app = Folder::new("drop", SHARED);
        let instance = runtime.load_instance(&app.0, "main.js").expect("instance");
        let window = cx.add_window(|_, _| Empty);
        let mut context = VisualTestContext::from_window(*window, cx);
        let view =
            context.update(|window, cx| instance.mount("List", None, window, cx).expect("list"));
        let handle = instance.mounted.borrow()[&view.entity_id()];
        let application = instance.application.clone();
        assert_eq!(runtime.app_modules.registration_count(), 1);

        drop(instance);
        assert!(!application.is_active());
        assert!(runtime.entities().view(handle).is_none());
        drop(view);
        context.update(|_, _| {});
        assert_eq!(runtime.app_modules.registration_count(), 0);
    }

    #[gpui::test]
    fn a_missing_or_mistyped_export_is_an_error_and_nothing_more(cx: &mut TestAppContext) {
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        let app = Folder::new("missing", SHARED);
        let instance = runtime.load_instance(&app.0, "main.js").expect("instance");
        let window = cx.add_window(|_, _| Empty);
        let mut context = VisualTestContext::from_window(*window, cx);

        context.update(|window, cx| {
            let error = instance.mount("Nowhere", None, window, cx).unwrap_err();
            assert!(error.to_string().contains("`Nowhere`"), "{error:#}");
            let error = instance.call("nowhere", vec![], window, cx).unwrap_err();
            assert!(error.to_string().contains("`nowhere`"), "{error:#}");
            let error = instance
                .call("notAFunction", vec![], window, cx)
                .unwrap_err();
            assert!(error.to_string().contains("`notAFunction`"), "{error:#}");
            let error = instance.call("List", vec![], window, cx).unwrap_err();
            assert!(!error.to_string().is_empty(), "a class is not callable");
        });
        assert!(instance.is_active());
        assert!(instance.mounted.borrow().is_empty());
        context.update(|_, cx| instance.unload(cx));
    }

    #[gpui::test]
    fn an_entry_needs_no_default_export_and_says_what_it_exports(cx: &mut TestAppContext) {
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        let app = Folder::new("exports", SHARED);
        let instance = runtime.load_instance(&app.0, "main.js").expect("instance");
        let mut exports = instance.exports().expect("exports");
        exports.sort();
        assert_eq!(exports, ["Detail", "List", "bump", "notAFunction", "probe"]);

        let Err(error) = runtime.load_application(&app.0, "main.js") else {
            panic!("an entry without a default export mounts nothing whole");
        };
        assert!(error.to_string().contains("export default"), "{error:#}");
        cx.update(|cx| instance.unload(cx));
    }

    #[gpui::test]
    fn a_throwing_init_leaves_the_instance_usable(cx: &mut TestAppContext) {
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        let app = Folder::new(
            "throwing",
            r#"
import { View } from "gpui-kit";
let built = 0;
export class Broken extends View {
  init() { throw new Error("broken init"); }
  render(cx) { return "never"; }
}
export class Fine extends View {
  init() { built += 1; }
  render(cx) { return "fine"; }
}
export function probe(cx) { globalThis.__probe = String(built); }
"#,
        );
        let instance = runtime.load_instance(&app.0, "main.js").expect("instance");
        let window = cx.add_window(|_, _| Empty);
        let mut context = VisualTestContext::from_window(*window, cx);

        context.update(|window, cx| {
            let fine = instance.mount("Fine", None, window, cx).expect("fine");
            let error = instance.mount("Broken", None, window, cx).unwrap_err();
            assert!(format!("{error:#}").contains("broken init"), "{error:#}");
            assert!(
                instance.is_active(),
                "a failed mount must not unload its siblings"
            );
            assert_eq!(instance.mounted.borrow().len(), 1);
            assert!(instance.mounted.borrow().contains_key(&fine.entity_id()));
            instance
                .mount("Fine", None, window, cx)
                .expect("fine again");
            instance.call("probe", vec![], window, cx).expect("probe");
        });
        assert_eq!(probe(&runtime), "2");
        context.update(|_, cx| instance.unload(cx));
    }

    #[gpui::test]
    fn an_async_export_runs_to_its_end_before_call_returns(cx: &mut TestAppContext) {
        let runtime = ShellRuntime::new_isolated().expect("runtime");
        let app = Folder::new(
            "async",
            r#"
export async function settle(word, cx) {
  globalThis.__probe = "started";
  await Promise.resolve();
  globalThis.__probe = `settled ${word}`;
}
"#,
        );
        let instance = runtime.load_instance(&app.0, "main.js").expect("instance");
        let window = cx.add_window(|_, _| Empty);
        let mut context = VisualTestContext::from_window(*window, cx);
        context.update(|window, cx| {
            instance
                .call("settle", vec![HostValue::Str("here".into())], window, cx)
                .expect("settle")
        });
        assert_eq!(probe(&runtime), "settled here");
        context.update(|_, cx| instance.unload(cx));
    }
}
