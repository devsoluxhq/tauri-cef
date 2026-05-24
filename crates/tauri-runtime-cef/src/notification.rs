//! CEF-only extension: native interception of Web Notifications.
//!
//! The renderer subprocess installs V8 shims for `window.Notification`,
//! `ServiceWorkerRegistration.prototype.showNotification`, and
//! `navigator.permissions.query({ name: "notifications" })`. Those shims
//! make web apps such as Slack observe a granted notification permission and
//! forward notification payloads over a `ProcessMessage` named
//! `"beypilot.notify"` to the browser process, where
//! `BrowserClient::on_process_message_received` decodes it and calls whatever
//! handler the embedder has registered for the originating browser id.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use cef::{rc::*, *};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationSource {
  /// Page called `new Notification(...)`.
  Window,
  /// Service worker called `registration.showNotification(...)`.
  ServiceWorker,
}

#[derive(Debug, Clone)]
pub struct NotificationPayload {
  pub source: NotificationSource,
  pub title: String,
  pub body: Option<String>,
  pub icon: Option<String>,
  pub tag: Option<String>,
  pub silent: bool,
  /// `frame.url()` at the time of the call. Useful for origin-based routing.
  pub origin: String,
}

pub type NotificationHandler = Arc<dyn Fn(NotificationPayload) + Send + Sync>;

pub(crate) const IPC_MESSAGE_NAME: &str = "beypilot.notify";

static REGISTRY: OnceLock<Mutex<HashMap<i32, NotificationHandler>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<i32, NotificationHandler>> {
  REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a handler keyed by CEF browser id (`cef::Browser::identifier()`).
/// Replaces any previously registered handler for the same browser.
pub fn register<F>(browser_id: i32, handler: F)
where
  F: Fn(NotificationPayload) + Send + Sync + 'static,
{
  registry()
    .lock()
    .unwrap()
    .insert(browser_id, Arc::new(handler));
}

pub fn unregister(browser_id: i32) {
  registry().lock().unwrap().remove(&browser_id);
}

/// Called by `BrowserClient` when a `"beypilot.notify"` IPC arrives.
pub(crate) fn dispatch(browser_id: i32, payload: NotificationPayload) {
  let handler = registry().lock().unwrap().get(&browser_id).cloned();
  if let Some(h) = handler {
    log::info!(
      "[cef-notify] dispatch browser_id={} source={:?} title={:?} origin={}",
      browser_id,
      payload.source,
      payload.title,
      payload.origin
    );
    h(payload);
  } else {
    log::warn!(
      "[cef-notify] dropped browser_id={} source={:?} title={:?} origin={} (no handler)",
      browser_id,
      payload.source,
      payload.title,
      payload.origin
    );
  }
}

// ---------------------------------------------------------------------------
// Renderer-side notification and permission shims.
// ---------------------------------------------------------------------------

wrap_app! {
  pub struct NotifyApp;

  impl App {
    fn render_process_handler(&self) -> Option<RenderProcessHandler> {
      Some(NotifyRenderProcessHandler::new())
    }
  }
}

wrap_render_process_handler! {
  pub struct NotifyRenderProcessHandler;

  impl RenderProcessHandler {
    fn on_context_created(
      &self,
      browser: Option<&mut Browser>,
      frame: Option<&mut Frame>,
      context: Option<&mut V8Context>,
    ) {
      let (Some(frame), Some(context)) = (frame, context) else { return; };

      let origin = CefString::from(&frame.url()).to_string();
      let browser_id = browser.map(|b| b.identifier()).unwrap_or(-1);
      log::info!(
        "[cef-render-notify] on_context_created browser_id={} origin={}",
        browser_id,
        origin
      );
      let Some(global) = context.global() else { return; };

      install_notification_shim(&global, &origin, NotificationSource::Window);
      install_sw_shim(&global, &origin);
      install_permissions_query_shim(context);
      log::info!(
        "[cef-render-notify] installed shims browser_id={} origin={}",
        browser_id,
        origin
      );
    }
  }
}

fn install_notification_shim(global: &V8Value, origin: &str, source: NotificationSource) {
  let mut handler = NotifyV8Handler::new(source, origin.to_owned());
  let Some(mut shim) =
    v8_value_create_function(Some(&CefString::from("Notification")), Some(&mut handler))
  else {
    return;
  };

  if let Some(mut perm) = v8_value_create_string(Some(&CefString::from("granted"))) {
    shim.set_value_bykey(
      Some(&CefString::from("permission")),
      Some(&mut perm),
      V8Propertyattribute::default(),
    );
  }

  let mut rp_handler = ResolveGrantedHandler::new();
  if let Some(mut rp_fn) = v8_value_create_function(
    Some(&CefString::from("requestPermission")),
    Some(&mut rp_handler),
  ) {
    shim.set_value_bykey(
      Some(&CefString::from("requestPermission")),
      Some(&mut rp_fn),
      V8Propertyattribute::default(),
    );
  }

  if let Some(mut marker) = v8_value_create_bool(1) {
    shim.set_value_bykey(
      Some(&CefString::from("__beypilot_cef")),
      Some(&mut marker),
      V8Propertyattribute::default(),
    );
  }

  global.set_value_bykey(
    Some(&CefString::from("Notification")),
    Some(&mut shim),
    V8Propertyattribute::default(),
  );

  // Stable debug hook: lets DevTools force the helper path even if the page
  // later overwrites `window.Notification`. Kept in sync with the macOS
  // bundled `cef-helper/src/notification.rs` copy of this shim so dev/Linux/
  // Windows (run via `run_cef_helper_process`) and the macOS bundled helper
  // expose the same renderer surface.
  global.set_value_bykey(
    Some(&CefString::from("__BEYPILOT_CEF_NOTIFICATION_CONSTRUCTOR")),
    Some(&mut shim),
    V8Propertyattribute::default(),
  );

  // Test/diagnostic entry point that fires a notification IPC directly,
  // bypassing any page-level wrapping of `window.Notification`.
  if let Some(mut fire_fn) = v8_value_create_function(
    Some(&CefString::from("__beypilotFireNotification")),
    Some(&mut handler),
  ) {
    global.set_value_bykey(
      Some(&CefString::from("__beypilotFireNotification")),
      Some(&mut fire_fn),
      V8Propertyattribute::default(),
    );
  }

  if let Some(mut marker) = v8_value_create_bool(1) {
    global.set_value_bykey(
      Some(&CefString::from("__BEYPILOT_CEF_NOTIFICATION_SHIM")),
      Some(&mut marker),
      V8Propertyattribute::default(),
    );
  }

  if let Some(mut origin_value) = v8_value_create_string(Some(&CefString::from(origin))) {
    global.set_value_bykey(
      Some(&CefString::from("__BEYPILOT_CEF_NOTIFICATION_ORIGIN")),
      Some(&mut origin_value),
      V8Propertyattribute::default(),
    );
  }
}

fn install_sw_shim(global: &V8Value, origin: &str) {
  let Some(sw_reg) = global.value_bykey(Some(&CefString::from("ServiceWorkerRegistration"))) else {
    return;
  };
  if sw_reg.is_object() == 0 {
    return;
  }
  let Some(proto) = sw_reg.value_bykey(Some(&CefString::from("prototype"))) else {
    return;
  };
  if proto.is_object() == 0 {
    return;
  }

  let mut handler = NotifyV8Handler::new(NotificationSource::ServiceWorker, origin.to_owned());
  let Some(mut shim) = v8_value_create_function(
    Some(&CefString::from("showNotification")),
    Some(&mut handler),
  ) else {
    return;
  };

  proto.set_value_bykey(
    Some(&CefString::from("showNotification")),
    Some(&mut shim),
    V8Propertyattribute::default(),
  );
}

wrap_v8_handler! {
  struct NotifyV8Handler {
    source: NotificationSource,
    origin: String,
  }

  impl V8Handler {
    fn execute(
      &self,
      _name: Option<&CefString>,
      _object: Option<&mut V8Value>,
      arguments: Option<&[Option<V8Value>]>,
      retval: Option<&mut Option<V8Value>>,
      _exception: Option<&mut CefString>,
    ) -> ::std::os::raw::c_int {
      let args = arguments.unwrap_or(&[]);

      let title = args
        .first()
        .and_then(|v| v.as_ref())
        .filter(|v| v.is_string() != 0)
        .map(|v| CefString::from(&v.string_value()).to_string())
        .unwrap_or_default();

      let opts = args.get(1).and_then(|v| v.as_ref()).filter(|v| v.is_object() != 0);

      let body = read_opt_str(opts, "body");
      let icon = read_opt_str(opts, "icon");
      let tag = read_opt_str(opts, "tag");
      let silent = opts
        .and_then(|o| o.value_bykey(Some(&CefString::from("silent"))))
        .map(|v| v.bool_value() != 0)
        .unwrap_or(false);

      if let Some(mut msg) = process_message_create(Some(&CefString::from(IPC_MESSAGE_NAME))) {
        if let Some(list) = msg.argument_list() {
          list.set_size(7);
          list.set_int(0, self.source as i32);
          list.set_string(1, Some(&CefString::from(title.as_str())));
          list.set_string(2, Some(&CefString::from(body.as_deref().unwrap_or(""))));
          list.set_string(3, Some(&CefString::from(icon.as_deref().unwrap_or(""))));
          list.set_string(4, Some(&CefString::from(tag.as_deref().unwrap_or(""))));
          list.set_int(5, if silent { 1 } else { 0 });
          list.set_string(6, Some(&CefString::from(self.origin.as_str())));
        }
        if let Some(ctx) = v8_context_get_current_context() {
          if let Some(frame) = ctx.frame() {
            frame.send_process_message(ProcessId::BROWSER, Some(&mut msg));
          }
        }
      }

      match self.source {
        NotificationSource::Window => {
          if let Some(retval) = retval {
            *retval = v8_value_create_object(None, None);
          }
        }
        NotificationSource::ServiceWorker => {
          if let Some(retval) = retval {
            if let Some(promise) = v8_value_create_promise() {
              if let Some(mut undef) = v8_value_create_undefined() {
                promise.resolve_promise(Some(&mut undef));
              }
              *retval = Some(promise);
            }
          }
        }
      }
      1
    }
  }
}

wrap_v8_handler! {
  struct ResolveGrantedHandler;

  impl V8Handler {
    fn execute(
      &self,
      _name: Option<&CefString>,
      _object: Option<&mut V8Value>,
      _arguments: Option<&[Option<V8Value>]>,
      retval: Option<&mut Option<V8Value>>,
      _exception: Option<&mut CefString>,
    ) -> ::std::os::raw::c_int {
      if let Some(retval) = retval {
        if let Some(promise) = v8_value_create_promise() {
          if let Some(mut granted) = v8_value_create_string(Some(&CefString::from("granted"))) {
            promise.resolve_promise(Some(&mut granted));
          }
          *retval = Some(promise);
        }
      }
      1
    }
  }
}

fn install_permissions_query_shim(context: &V8Context) {
  let js = concat!(
    "(function(){",
    "try{",
    "var p=navigator&&navigator.permissions;",
    "if(!p||typeof p.query!=='function')return;",
    "var q=p.query.bind(p);",
    "var f={query:function(d){",
    "if(d&&d.name==='notifications')",
    "return Promise.resolve({state:'granted',onchange:null});",
    "return q(d);",
    "}};",
    "Object.defineProperty(navigator,'permissions',",
    "{get:function(){return f;},configurable:true});",
    "}catch(_){}",
    "})();"
  );
  let mut retval: Option<V8Value> = None;
  let mut exception: Option<V8Exception> = None;
  context.eval(
    Some(&CefString::from(js)),
    // Keep this script-source label in sync with the macOS `cef-helper` copy
    // (`cef-helper/src/notification.rs`). Note: `cef-helper` logs via
    // `eprintln!` rather than `log::` because that standalone crate has no
    // `log` dependency — that divergence is intentional, this URL was not.
    Some(&CefString::from("beypilot://notification-perm-shim")),
    0,
    Some(&mut retval),
    Some(&mut exception),
  );
}

fn read_opt_str(obj: Option<&V8Value>, key: &str) -> Option<String> {
  let v = obj?.value_bykey(Some(&CefString::from(key)))?;
  if v.is_string() != 0 {
    Some(CefString::from(&v.string_value()).to_string())
  } else {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The renderer shim (`NotifyV8Handler::execute`) and the browser-process
  /// decoder (`BrowserClient::on_process_message_received` in `cef_impl.rs`)
  /// must agree on the IPC message name, and the macOS bundled
  /// `cef-helper/src/notification.rs` copy hard-codes the same value in its
  /// `IPC_NAME` constant. Drift here makes notifications silently stop
  /// dispatching, so pin the wire name.
  #[test]
  fn ipc_message_name_is_stable() {
    assert_eq!(IPC_MESSAGE_NAME, "beypilot.notify");
  }

  /// Locks the argument-list layout shared with the decoder and the
  /// `cef-helper` copy: 7 fields, `source` encoded as an int with
  /// `Window == 0` / `ServiceWorker == 1`.
  ///
  /// Layout: [0]=source(int) [1]=title [2]=body [3]=icon [4]=tag
  ///         [5]=silent(int) [6]=origin
  #[test]
  fn ipc_source_discriminants_match_wire_contract() {
    assert_eq!(NotificationSource::Window as i32, 0);
    assert_eq!(NotificationSource::ServiceWorker as i32, 1);
  }

  /// Guards against drift between this renderer shim and the macOS bundled copy
  /// in `cef-helper/src/notification.rs`. The two live in separate workspaces and
  /// can't share a module, so we embed the helper source at compile time and
  /// assert the contract-critical tokens are present in both. If the helper is
  /// edited out of sync (IPC name, arg layout, debug globals, perm-shim URL),
  /// this test fails.
  #[test]
  fn cef_helper_shim_matches_ipc_contract() {
    const HELPER_SRC: &str = include_str!("../../../cef-helper/src/notification.rs");

    assert!(
      HELPER_SRC.contains(IPC_MESSAGE_NAME),
      "cef-helper must use the same IPC name ({IPC_MESSAGE_NAME})"
    );
    assert!(
      HELPER_SRC.contains("list.set_size(7)"),
      "cef-helper notification arg-list arity drifted from 7"
    );
    for token in [
      "__BEYPILOT_CEF_NOTIFICATION_CONSTRUCTOR",
      "__beypilotFireNotification",
      "__BEYPILOT_CEF_NOTIFICATION_SHIM",
      "__BEYPILOT_CEF_NOTIFICATION_ORIGIN",
      "beypilot://notification-perm-shim",
    ] {
      assert!(
        HELPER_SRC.contains(token),
        "cef-helper notification shim is missing `{token}` — shims have drifted"
      );
    }
  }
}
