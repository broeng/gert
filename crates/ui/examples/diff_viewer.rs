//! Dev-only: shows the diff widget on a fixture from `tests/fixtures`.
//!
//! ```text
//! cargo run -p ui --example diff_viewer [FIXTURE] [--unified] [--inline] [--tab N] [--font NAME] [--path FILE] [--light|--dark]
//! ```
//!
//! `FIXTURE` is a path, or a name such as `diff`, `large_diff` or
//! `diff_rename` (looked up in `tests/fixtures/` and
//! `tests/fixtures/handwritten/`, with `.response.json` appended).
//! `--path` names the file the diff is of (e.g. `src/lib.rs`), which picks
//! the syntax highlighting; without it the diff is drawn plain.
//! Expansion requests are only logged: there is no server to ask.
//!
//! The fixture goes through `GerritClient` and a fake HTTP client, because
//! the wire types are private to `gerrit-api`; that is the same path the
//! app takes, minus the network.

use std::{path::PathBuf, sync::Arc};

use gerrit_api::{Auth, DiffParams, GerritClient, fake::FakeHttpClient};
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::*;
use model::{ChangeNumber, DiffViewMode, ResolvedDiffPreferences, RevisionId};
use ui::diff::{DiffEvent, DiffSizing, DiffView};

fn find_fixture(name: &str) -> Result<PathBuf, String> {
    let direct = PathBuf::from(name);
    if direct.is_file() {
        return Ok(direct);
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    for dir in [root.clone(), root.join("handwritten")] {
        for file in [format!("{name}.response.json"), name.to_owned()] {
            let p = dir.join(file);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(format!("no fixture {name:?}"))
}

fn load(path: &PathBuf) -> Result<model::Diff, String> {
    let body = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let fake = FakeHttpClient::new();
    fake.push_response(200, body);
    let client = GerritClient::new(Arc::new(fake), "https://gerrit.invalid", Auth::Anonymous)
        .map_err(|e| e.to_string())?;
    futures::executor::block_on(client.get_diff(
        ChangeNumber(1),
        &RevisionId::from("0000"),
        "file",
        &DiffParams::default(),
    ))
    .map_err(|e| e.to_string())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut name = "diff".to_owned();
    let mut prefs = ResolvedDiffPreferences::default();
    let mut inline = false;
    let mut font = None;
    let mut path: Option<String> = None;
    let mut mode = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--unified" => prefs.view_mode = DiffViewMode::Unified,
            "--inline" => inline = true,
            "--tab" => prefs.tab_size = args.next().and_then(|n| n.parse().ok()).unwrap_or(4),
            "--font" => font = args.next(),
            "--path" => path = args.next(),
            "--light" => mode = Some(ThemeMode::Light),
            "--dark" => mode = Some(ThemeMode::Dark),
            other => name = other.to_owned(),
        }
    }
    let diff = match find_fixture(&name).and_then(|p| load(&p)) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("diff_viewer: {e}");
            std::process::exit(2);
        }
    };

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        ui::init(cx);
        if let Some(m) = mode {
            Theme::change(m, None, cx);
        }
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1200.), px(760.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some("diff viewer".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            if mode.is_none() {
                Theme::sync_system_appearance(Some(window), cx);
            }
            let view = cx.new(|cx| {
                let mut v = DiffView::new(diff, prefs, cx);
                if inline {
                    v.set_sizing(DiffSizing::Inline { max_rows: Some(30) }, cx);
                }
                v.set_font_family(font.map(Into::into), cx);
                if let Some(path) = &path {
                    v.set_path(path, cx);
                }
                v
            });
            cx.subscribe(&view, |_, ev: &DiffEvent, _| match ev {
                DiffEvent::RowsChanged => eprintln!("rows changed"),
                DiffEvent::ModeChanged(m) => eprintln!("mode: {m:?}"),
                DiffEvent::Draft(req) => eprintln!("draft request {req:?}"),
                DiffEvent::Highlighted(_) => {}
            })
            .detach();
            let focus = view.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            view
        });
        match opened {
            Ok(_) => {
                cx.on_window_closed(|cx, _| {
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                })
                .detach();
                cx.activate(true);
            }
            Err(err) => {
                eprintln!("failed to open the window: {err:#}");
                cx.quit();
            }
        }
    });
}
