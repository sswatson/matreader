//! Drive the TUI through every variable of the fixtures with a TestBackend to
//! make sure each view renders without panicking.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use matreader::app::{App, View};
use matreader::ui;
use ratatui::{backend::TestBackend, Terminal};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn render(term: &mut Terminal<TestBackend>, app: &mut App) -> String {
    term.draw(|f| ui::draw(f, app)).unwrap();
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Press a key and let any background job finish.
fn press(term: &mut Terminal<TestBackend>, app: &mut App, code: KeyCode) -> String {
    app.handle_key(key(code), 20);
    let start = Instant::now();
    loop {
        app.tick();
        if !app.is_loading() || start.elapsed() > Duration::from_secs(20) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!app.is_loading(), "load did not finish");
    render(term, app)
}

fn walk(name: &str) {
    let file = matreader::open(&fixture(name)).unwrap();
    let nvars = file.variables().len();
    let mut app = App::new(file, name.to_string());
    let mut term = Terminal::new(TestBackend::new(140, 40)).unwrap();
    let first = render(&mut term, &mut app);
    assert!(first.contains("Variables"), "{}", first);

    for i in 0..nvars {
        // Select variable i from the top.
        app.handle_key(key(KeyCode::Char('g')), 20);
        for _ in 0..i {
            app.handle_key(key(KeyCode::Char('j')), 20);
        }
        let var_name = app.file.variables()[i].name.clone();
        let screen = press(&mut term, &mut app, KeyCode::Enter);
        assert_eq!(
            app.views.len(),
            2,
            "opening {} did not push a view:\n{}",
            var_name,
            screen
        );
        assert!(
            screen.contains(&var_name),
            "breadcrumb missing for {}:\n{}",
            var_name,
            screen
        );

        // Exercise view-specific keys and drill one level further where possible.
        match app.current() {
            View::Grid(_) => {
                for c in ['j', 'l', 'L', 'G', '$', ']', '[', 'g', '0'] {
                    press(&mut term, &mut app, KeyCode::Char(c));
                }
                press(&mut term, &mut app, KeyCode::PageDown);
            }
            View::Text(_) => {
                press(&mut term, &mut app, KeyCode::Char('j'));
                press(&mut term, &mut app, KeyCode::Char('l'));
            }
            View::Sparse(_) => {
                press(&mut term, &mut app, KeyCode::Char('j'));
                press(&mut term, &mut app, KeyCode::Char('G'));
            }
            View::Container(_) => {
                press(&mut term, &mut app, KeyCode::Char('j'));
                let depth = app.views.len();
                let s = press(&mut term, &mut app, KeyCode::Enter);
                if app.views.len() > depth {
                    // Went one level deeper; look around and come back.
                    press(&mut term, &mut app, KeyCode::Char('j'));
                    if let View::Container(_) = app.current() {
                        let d2 = app.views.len();
                        press(&mut term, &mut app, KeyCode::Enter);
                        if app.views.len() > d2 {
                            press(&mut term, &mut app, KeyCode::Esc);
                        }
                    }
                    press(&mut term, &mut app, KeyCode::Esc);
                } else {
                    assert!(
                        app.status.is_none() || !app.status.as_ref().unwrap().1,
                        "error opening child of {}:\n{}",
                        var_name,
                        s
                    );
                }
                // Filter round-trip.
                press(&mut term, &mut app, KeyCode::Char('/'));
                press(&mut term, &mut app, KeyCode::Char('a'));
                press(&mut term, &mut app, KeyCode::Enter);
                press(&mut term, &mut app, KeyCode::Esc);
            }
            View::Info(_) => {
                let depth = app.views.len();
                press(&mut term, &mut app, KeyCode::Enter);
                if app.views.len() > depth {
                    press(&mut term, &mut app, KeyCode::Esc);
                }
            }
            View::Variables { .. } => unreachable!(),
        }
        while app.views.len() > 1 {
            press(&mut term, &mut app, KeyCode::Esc);
        }
        if let Some((msg, true, _)) = &app.status {
            panic!("error status after visiting {}: {}", var_name, msg);
        }
    }
    // Help overlay and filter on the variables list.
    let s = press(&mut term, &mut app, KeyCode::Char('?'));
    assert!(s.contains("Help"));
    press(&mut term, &mut app, KeyCode::Esc);
    press(&mut term, &mut app, KeyCode::Char('/'));
    press(&mut term, &mut app, KeyCode::Char('s'));
    let s = press(&mut term, &mut app, KeyCode::Enter);
    assert!(s.contains("filter: s"), "{}", s);
    press(&mut term, &mut app, KeyCode::Esc);
    press(&mut term, &mut app, KeyCode::Char('q'));
    assert!(app.should_quit);
}

#[test]
fn walk_v7() {
    walk("zoo_v7.mat");
}

#[test]
fn walk_v6() {
    walk("zoo_v6.mat");
}

#[test]
fn walk_v73() {
    walk("zoo_v73.mat");
}

#[test]
fn walk_v73_nocomp() {
    walk("zoo_v73_nocomp.mat");
}

#[test]
fn screenshots() {
    // Print a few screens so a human can eyeball the layout in test output.
    let file = matreader::open(&fixture("zoo_v7.mat")).unwrap();
    let mut app = App::new(file, "zoo_v7.mat".to_string());
    let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
    println!("{}", render(&mut term, &mut app));
    let idx = |name: &str, app: &App| {
        app.file
            .variables()
            .iter()
            .position(|v| v.name == name)
            .unwrap()
    };
    for name in [
        "matrix",
        "scalarStruct",
        "cellArr",
        "charMat",
        "sparseMat",
        "nd3",
        "strScalar",
    ] {
        let i = idx(name, &app);
        app.handle_key(key(KeyCode::Char('g')), 20);
        for _ in 0..i {
            app.handle_key(key(KeyCode::Char('j')), 20);
        }
        let s = press(&mut term, &mut app, KeyCode::Enter);
        println!("--- {} ---\n{}", name, s);
        if name == "scalarStruct" {
            press(&mut term, &mut app, KeyCode::Char('j'));
            press(&mut term, &mut app, KeyCode::Char('j'));
            let s = press(&mut term, &mut app, KeyCode::Enter);
            println!("--- scalarStruct.nested ---\n{}", s);
            press(&mut term, &mut app, KeyCode::Esc);
        }
        press(&mut term, &mut app, KeyCode::Esc);
    }
}

/// Open variable `name` from the top-level list.
fn open_var(term: &mut Terminal<TestBackend>, app: &mut App, name: &str) -> String {
    let i = app
        .file
        .variables()
        .iter()
        .position(|v| v.name == name)
        .unwrap();
    app.handle_key(key(KeyCode::Char('g')), 20);
    for _ in 0..i {
        app.handle_key(key(KeyCode::Char('j')), 20);
    }
    press(term, app, KeyCode::Enter)
}

#[test]
fn display_options() {
    let name = "zoo_v7.mat";
    let mut app = App::new(matreader::open(&fixture(name)).unwrap(), name.into());
    let mut term = Terminal::new(TestBackend::new(140, 40)).unwrap();

    let screen = open_var(&mut term, &mut app, "scalarDouble");
    assert!(screen.contains("3.14159 "), "{}", screen);
    for _ in 0..4 {
        press(&mut term, &mut app, KeyCode::Char('+'));
    }
    let screen = render(&mut term, &mut app);
    assert!(screen.contains("3.141592654"), "{}", screen);
    assert!(screen.contains("10 digits"), "{}", screen);
    for _ in 0..8 {
        press(&mut term, &mut app, KeyCode::Char('-'));
    }
    let screen = render(&mut term, &mut app);
    assert!(screen.contains("3.1 "), "{}", screen);
    assert!(screen.contains("2 digits"), "{}", screen);

    // Datenum mode: rowVec = 1:10 are datenums 0000-01-01 .. 0000-01-10.
    press(&mut term, &mut app, KeyCode::Esc);
    let screen = open_var(&mut term, &mut app, "rowVec");
    assert!(!screen.contains("0000-01-01"), "{}", screen);
    let screen = press(&mut term, &mut app, KeyCode::Char('D'));
    assert!(screen.contains("0000-01-10"), "{}", screen);
    assert!(
        screen.contains("0000-01-01 00:00:00.000  (datenum 1)"),
        "{}",
        screen
    );
    assert!(screen.contains("dates"), "{}", screen);
    if std::env::var("SHOW_SCREENS").is_ok() {
        println!("{}", screen);
    }

    // The option also applies to (and refreshes) container previews.
    press(&mut term, &mut app, KeyCode::Esc);
    let screen = open_var(&mut term, &mut app, "cellArr");
    assert!(screen.contains("0000-01-01"), "{}", screen);
    let screen = press(&mut term, &mut app, KeyCode::Char('D'));
    assert!(!screen.contains("0000-01-01"), "{}", screen);
}
