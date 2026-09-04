//! Timing probe: open a file, load one variable, read a few small windows.
//!
//!     cargo run --release --example bench -- FILE [VARIABLE]
use matreader::model::Window;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: bench FILE [VAR]");
    let var = std::env::args().nth(2);
    let t = Instant::now();
    let file = matreader::open(std::path::Path::new(&path)).unwrap();
    println!(
        "open + index: {:?}  ({} variables, {})",
        t.elapsed(),
        file.variables().len(),
        file.format()
    );
    let vars: Vec<String> = match var {
        Some(v) => vec![v],
        None => file.variables().iter().map(|v| v.name.clone()).collect(),
    };
    for name in vars {
        let idx = file
            .variables()
            .iter()
            .position(|v| v.name == name)
            .expect("variable");
        let info = &file.variables()[idx];
        let t = Instant::now();
        let node = match file.load(idx, &AtomicBool::new(false)) {
            Ok(n) => n,
            Err(e) => {
                println!("{}: load failed: {}", name, e);
                continue;
            }
        };
        let load_t = t.elapsed();
        let shape = node.shape();
        let mut msg = format!(
            "{:<14} {:>12} {:<8} load {:>10.2?}",
            name,
            shape.to_string(),
            info.class.name(),
            load_t
        );
        match node.kind() {
            matreader::model::Kind::Numeric if !shape.is_empty() => {
                // Read a 40x12 window in the middle and one at the end.
                let r0 = shape.rows() / 2;
                let c0 = shape.cols() / 2;
                let page = vec![0; shape.ndims().saturating_sub(2)];
                let t = Instant::now();
                let w = Window::page(
                    r0..(r0 + 40).min(shape.rows()),
                    c0..(c0 + 12).min(shape.cols()),
                    &page,
                )
                .clamp(&shape);
                let v = node.read_window(&w).unwrap();
                let t1 = t.elapsed();
                let t = Instant::now();
                let w2 = Window::page(
                    shape.rows().saturating_sub(40)..shape.rows(),
                    shape.cols().saturating_sub(12)..shape.cols(),
                    &page,
                )
                .clamp(&shape);
                let v2 = node.read_window(&w2).unwrap();
                let t2 = t.elapsed();
                msg.push_str(&format!(
                    "  window(mid) {:>9.2?} [{} vals, first={:?}]  window(end) {:>9.2?} [{} vals]",
                    t1,
                    v.len(),
                    v.first().map(|s| s.as_f64()),
                    t2,
                    v2.len()
                ));
            }
            matreader::model::Kind::Cell => {
                let t = Instant::now();
                let n = shape.numel();
                let c = node.cell(n / 2).unwrap();
                msg.push_str(&format!(
                    "  cell({}) {:>9.2?} -> {}",
                    n / 2,
                    t.elapsed(),
                    matreader::model::summarize(&*c)
                ));
            }
            matreader::model::Kind::Struct if shape.numel() == 1 => {
                let fields = node.field_names().unwrap_or_default();
                msg.push_str(&format!("  {} fields:", fields.len()));
                for (f, name) in fields.iter().enumerate().take(12) {
                    let t = Instant::now();
                    match node.field(0, f) {
                        Ok(c) => msg.push_str(&format!(
                            "\n      .{:<28} {:<28} {:>9.2?}",
                            name,
                            matreader::model::summarize(&*c),
                            t.elapsed()
                        )),
                        Err(e) => msg.push_str(&format!("\n      .{}: error {}", name, e)),
                    }
                }
            }
            matreader::model::Kind::Struct if shape.numel() > 1 => {
                let t = Instant::now();
                let n = shape.numel();
                let c = node.field(n / 2, 0).unwrap();
                msg.push_str(&format!(
                    "  field({},0) {:>9.2?} -> {}",
                    n / 2,
                    t.elapsed(),
                    matreader::model::summarize(&*c)
                ));
            }
            _ => {}
        }
        println!("{}", msg);
    }
}
