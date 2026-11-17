//! CADCraft hosted inside Workbench's side panel: the same app as the desktop build, drawn into a view
//! Workbench owns, with Workbench's look and only the drawing area showing (Hands off) until the user asks
//! for the menus. Workbench (and Claude) drive it through its control channel. See `craft-embed`.
//!
//! The control channel is the app's own (`docs/control-protocol.md`: `engine.execute`, `engine.commands`,
//! `cmdline.*`, `drawing.inspect`, `ui.render`...), plus Workbench's file operations by real path, as methods or
//! as `engine.execute` commands: `file.open {path}`, `file.new {metric?}`, `file.saveAs {path, version?}`.
//! Workbench holds one drawing per editor, so opening or starting one closes the others (Workbench saves first).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use cadcraft_engine::Session;
use cadcraft_ui_egui::control::{self, ControlRequest, Outcome};
use cadcraft_ui_egui::{CadApp, Services};
use craft_embed::{EmbedApp, egui, egui_wgpu};
use serde_json::{Value, json};

struct Cad(CadApp);

impl EmbedApp for Cad {
    fn logic(&mut self, ctx: &egui::Context) {
        self.0.guarded_logic(ctx);
    }
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.0.raw_input_hook(ctx, raw);
        cadcraft_ui_egui::cmdline::capture_tab(ctx, raw);
    }
    fn ui(&mut self, ui: &mut egui::Ui) {
        self.0.guarded_ui(ui);
    }
    fn control(&mut self, ctx: &egui::Context, request: &str) -> String {
        let req: Value = serde_json::from_str(request).unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().filter(|p| !p.is_null()).unwrap_or_else(|| json!({}));
        // Workbench's file operations also come as engine commands (`exec file.open`)
        let command = (method == "engine.execute").then(|| params.get("command").and_then(Value::as_str)).flatten();
        let (method, params) = match command {
            Some(c @ ("file.open" | "file.new" | "file.saveAs")) => {
                (c.to_owned(), params.get("params").cloned().filter(|p| !p.is_null()).unwrap_or_else(|| json!({})))
            }
            _ => (method.to_owned(), params),
        };
        let reply = match method.as_str() {
            "file.open" => wrap(self.open(&params)),
            "file.new" => wrap(self.fresh(&params)),
            "file.saveAs" => wrap(self.save_as(&params)),
            _ => self.app_control(ctx, &method, params),
        };
        reply.to_string()
    }
    fn gpu(&mut self, rs: &egui_wgpu::RenderState) {
        // What the desktop's app creator does with `cc.wgpu_render_state`: the GPU canvas (no stencil buffer
        // in the host's render pass, so clipped viewports clip without one)
        self.0.set_wgpu(rs, None);
    }
    fn exit(&mut self) {
        self.0.cancel_screenshots();
    }
}

impl Cad {
    /// The app's own control channel, with what Workbench reads from `ui.inspect` added.
    fn app_control(&mut self, ctx: &egui::Context, method: &str, params: Value) -> Value {
        let (req, _reply) = ControlRequest::new(method, params);
        let mut reply = match control::handle(&mut self.0, ctx, &req) {
            Outcome::Done(v) => v,
            Outcome::Screenshot { .. } => json!({"ok": false, "error": "Workbench takes the screenshot of the panel itself"}),
        };
        // Workbench reads unsaved changes from `documents[0].dirty`: the drawing on screen first
        if method == "ui.inspect"
            && let Some(result) = reply.get_mut("result").and_then(Value::as_object_mut)
        {
            result.insert("documents".into(), self.documents());
        }
        reply
    }

    /// The open drawings, the active one first.
    fn documents(&self) -> Value {
        let summary = self.0.session.summary();
        let mut docs: Vec<Value> = summary.get("documents").and_then(Value::as_array).cloned().unwrap_or_default();
        docs.sort_by_key(|d| !d.get("active").and_then(Value::as_bool).unwrap_or(false));
        Value::Array(docs)
    }

    /// `file.open {path}`: the drawing at `path`, as the only one open, fitted to the view.
    fn open(&mut self, p: &Value) -> Result<Value, String> {
        let path = p.get("path").and_then(Value::as_str).ok_or("file.open: `path` is required")?;
        let r = self.0.run("open", json!({ "path": path }))?;
        self.keep_only_active();
        let _ = self.0.session.zoom_extents();
        Ok(r)
    }

    /// `file.new {metric?}`: a blank drawing (millimetres unless `metric` is false), as the only one open.
    fn fresh(&mut self, p: &Value) -> Result<Value, String> {
        let metric = p.get("metric").and_then(Value::as_bool).unwrap_or(true);
        let r = self.0.run("new", json!({ "metric": metric }))?;
        self.keep_only_active();
        Ok(r)
    }

    /// `file.saveAs {path, version?}`: written by the app, in the format the name says (DXF or DWG).
    fn save_as(&mut self, p: &Value) -> Result<Value, String> {
        if p.get("path").and_then(Value::as_str).is_none_or(str::is_empty) {
            return Err("file.saveAs: `path` is required".into());
        }
        self.0.run("saveas", p.clone())
    }

    /// Close every drawing but the active one.
    fn keep_only_active(&mut self) {
        let keep = self.0.session.active;
        for i in (0..self.0.session.docs.len()).rev() {
            if i != keep {
                let _ = self.0.session.execute("close", &json!({ "index": i }));
            }
        }
    }
}

fn wrap(r: Result<Value, String>) -> Value {
    match r {
        Ok(v) => json!({"ok": true, "result": v}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// File formats for the engine, as the desktop app installs them (it stays I/O-agnostic).
fn install_io() {
    use cadcraft_engine::cmd::file;
    file::set_io(file::IoHooks {
        read: |b, name| cadcraft_io::read(b, name).map_err(|e| e.to_string()),
        write: |d, name| cadcraft_io::write(d, name).map_err(|e| e.to_string()),
        plot: Some(|d, space, opts| cadcraft_io::plotter::plot(d, space, opts).map_err(|e| e.to_string())),
    });
    file::set_image_decoder(|b| cadcraft_io::raster::decode(b).map_err(|e| e.to_string()));
    file::set_write_as(|d, name, o| {
        cadcraft_io::write_with(d, name, &cadcraft_io::SaveOptions { version: o.version, binary: o.binary }).map_err(|e| e.to_string())
    });
    // AutoLISP's entget, entmake and entmod see entities as DXF groups.
    file::set_entity_codec(file::EntityCodec { groups: cadcraft_io::entity_groups, entity: cadcraft_io::entity_from_groups });
}

fn make(_ctx: &egui::Context) -> Box<dyn EmbedApp> {
    install_io();
    // Files by path, read and written by the engine itself; no file dialogs (Workbench owns the files), no
    // prefs, autosave or update check
    let mut session = Session::empty();
    session.new_drawing(true);
    let mut app = CadApp::new(session, Services::default());
    // Workbench's look and Hands off from the first frame; Workbench sets the mode's accent right after
    let _ = app.run("view.workbench", json!({"on": true}));
    let _ = app.run("view.handsOff", json!({"on": true}));
    Box::new(Cad(app))
}

craft_embed::export_embed!(make);
