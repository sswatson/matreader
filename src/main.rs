use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use matreader::app::App;
use matreader::format::format_bytes;
use matreader::ui;
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "matreader")]
#[command(version)]
#[command(about = "Explore MATLAB .mat files (v5/v6/v7 and v7.3) lazily in a TUI", long_about = None)]
struct Args {
    /// The .mat file to open
    file: PathBuf,

    /// Print the top-level variables and exit instead of starting the TUI
    #[arg(short, long)]
    list: bool,
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Restores the terminal on every exit path so a panic never strands the
/// user in raw mode on the alternate screen.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));
}

fn main() -> Result<()> {
    let args = Args::parse();
    let file = match matreader::open(&args.file) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("matreader: {}: {:#}", args.file.display(), e);
            std::process::exit(1);
        }
    };

    if args.list {
        println!("{}", file.header_text());
        println!("{}", file.format());
        println!();
        let vars = file.variables();
        let name_w = vars.iter().map(|v| v.name.len()).max().unwrap_or(4).max(4);
        let class_w = vars
            .iter()
            .map(|v| v.class.name().len())
            .max()
            .unwrap_or(5)
            .max(5);
        println!(
            "{:<name_w$}  {:<class_w$}  {:<14}  {:>10}  ",
            "Name", "Class", "Size", "Bytes"
        );
        for v in vars {
            let mut flags = Vec::new();
            if v.is_sparse {
                flags.push("sparse");
            }
            if v.is_complex {
                flags.push("complex");
            }
            if v.is_global {
                flags.push("global");
            }
            if v.compressed {
                flags.push("compressed");
            }
            println!(
                "{:<name_w$}  {:<class_w$}  {:<14}  {:>10}  {}",
                v.name,
                v.class.name(),
                v.shape.to_string(),
                if v.storage_bytes > 0 {
                    format_bytes(v.storage_bytes)
                } else {
                    String::new()
                },
                flags.join(" ")
            );
        }
        return Ok(());
    }

    install_panic_hook();
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(file, args.file.display().to_string());
    let result = run(&mut terminal, &mut app);
    drop(_guard);
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        if app.should_quit {
            return Ok(());
        }
        let page = ui::main_area_rows(
            terminal
                .size()
                .map(|s| ratatui::layout::Rect::new(0, 0, s.width, s.height))?,
        );
        if event::poll(Duration::from_millis(60))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.handle_key(key, page),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        app.tick();
    }
}
