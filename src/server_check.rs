//! `edittr --check-servers [--bundled] [--only rs,c,go,ts,py]`: a real go-to-definition round trip against
//! each language server, on a tiny sample project. Used by the release workflow to
//! check that the installed / unpacked full packages can run their bundled servers,
//! and by the ignored `lsp_round_trip_*` tests.
//!
//! `--bundled` only accepts servers from the bundle next to the executable (not PATH
//! or on-demand installs), so a CI runner's own tools can't hide a broken bundle.

use crate::goto::{self, Outcome, Request};
use crate::lsp;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Sample {
    pub language: &'static str,
    pub ext: &'static str,
    pub files: &'static [(&'static str, &'static str)],
    pub main: &'static str,
    /// Text whose first character is where go to definition is asked.
    pub symbol: &'static str,
    /// Expected target: path relative to the project and 0-based line.
    pub expect: (&'static str, usize),
}

pub const SAMPLES: &[Sample] = &[
    Sample {
        language: "Rust",
        ext: "rs",
        files: &[
            ("Cargo.toml", "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            (
                "src/util.rs",
                "pub struct Point { pub x: i32 }\n\npub fn make() -> Point { Point { x: 1 } }\n",
            ),
            (
                "src/main.rs",
                "mod util;\nfn main() {\n    let p = util::make();\n    println!(\"{}\", p.x);\n}\n",
            ),
        ],
        main: "src/main.rs",
        symbol: "make()",
        expect: ("src/util.rs", 2),
    },
    Sample {
        language: "C/C++",
        ext: "c",
        files: &[
            ("util.h", "#pragma once\nint add(int a, int b);\n"),
            ("util.c", "#include \"util.h\"\n\nint add(int a, int b) { return a + b; }\n"),
            (
                "main.c",
                "#include \"util.h\"\nint main(void) {\n  int total = add(1, 2);\n  return total;\n}\n",
            ),
            ("compile_flags.txt", "-I.\n"),
        ],
        main: "main.c",
        symbol: "add(1",
        expect: ("util.h", 1), // the definition is in another TU, so clangd reports the declaration
    },
    Sample {
        language: "Go",
        ext: "go",
        files: &[
            ("go.mod", "module example.com/t\n\ngo 1.21\n"),
            (
                "util.go",
                "package main\n\n// Greet says hi.\nfunc Greet(name string) string { return \"hi \" + name }\n",
            ),
            (
                "main.go",
                "package main\n\nimport \"fmt\"\n\nfunc main() { fmt.Println(Greet(\"x\")) }\n",
            ),
        ],
        main: "main.go",
        symbol: "Greet(\"x\")",
        expect: ("util.go", 3),
    },
    Sample {
        language: "TypeScript",
        ext: "ts",
        files: &[
            ("tsconfig.json", "{ \"compilerOptions\": { \"strict\": true } }\n"),
            (
                "util.ts",
                "export const VERSION = 1;\n\nexport function greet(name: string) {\n  return `hi ${name}`;\n}\n",
            ),
            ("main.ts", "import { greet } from \"./util\";\n\nconsole.log(greet(\"x\"));\n"),
        ],
        main: "main.ts",
        symbol: "greet(\"x\")",
        expect: ("util.ts", 2),
    },
    Sample {
        language: "Python",
        ext: "py",
        files: &[
            ("util.py", "import os\n\n\ndef greet(name):\n    return f\"hi {name}\"\n"),
            ("main.py", "from util import greet\n\nprint(greet(\"x\"))\n"),
        ],
        main: "main.py",
        symbol: "greet(\"x\")",
        expect: ("util.py", 3),
    },
];

#[cfg(test)]
pub fn sample(ext: &str) -> &'static Sample {
    SAMPLES.iter().find(|s| s.ext == ext).expect("no sample for this extension")
}

/// Run go to definition on `sample` with the first server found; returns the server's
/// name and how it was launched, or what went wrong.
pub fn round_trip(sample: &Sample, bundled_only: bool, timeout: Duration) -> Result<(String, String), String> {
    let found = lsp::servers_for(sample.ext).into_iter().find_map(|spec| {
        let launch = if bundled_only {
            crate::bundled::find(spec.command)
        } else {
            lsp::resolve(spec.command)
        };
        launch.map(|l| (spec, l))
    });
    let Some((spec, command)) = found else {
        return Err(if bundled_only { "not in the bundle" } else { "not installed" }.into());
    };
    let shown = std::iter::once(command.exe.display().to_string())
        .chain(command.args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");

    let dir = std::env::temp_dir().join(format!("edittr-check-{}-{}", sample.ext, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (name, body) in sample.files {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(p, body).map_err(|e| e.to_string())?;
    }
    let result = (|| {
        let dir = std::fs::canonicalize(&dir).map_err(|e| e.to_string())?;
        let launch = Arc::new(lsp::Launch::default());
        launch.finish(Some(lsp::Client::start(&spec, command, &dir, &[], None)?));
        let text = std::fs::read_to_string(dir.join(sample.main)).map_err(|e| e.to_string())?;
        let idx = text.find(sample.symbol).map(|b| text[..b].chars().count()).unwrap() + 1;
        // Indexing can take a moment: retry until the server answers with a location.
        let start = Instant::now();
        let target = loop {
            let req = Request {
                root: dir.clone(),
                files: Box::new(Vec::new),
                path: dir.join(sample.main),
                text: text.clone(),
                ignored: Vec::new(),
                char_idx: idx,
                client: Some(launch.clone()),
                revision: (0, 0),
            };
            match goto::run(req) {
                Outcome::Found { target, via } if via == spec.name => break target,
                _ if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(500)),
                Outcome::NotFound(why) => return Err(format!("{} never answered ({why})", spec.name)),
                Outcome::Found { via, .. } => return Err(format!("{} never answered (only {via} did)", spec.name)),
            }
        };
        let rel = target
            .path
            .strip_prefix(&dir)
            .unwrap_or(&target.path)
            .to_string_lossy()
            .replace('\\', "/");
        if (rel.as_str(), target.line) == sample.expect {
            Ok(())
        } else {
            Err(format!(
                "went to {rel}:{} instead of {}:{}",
                target.line + 1,
                sample.expect.0,
                sample.expect.1 + 1
            ))
        }
    })();
    std::fs::remove_dir_all(&dir).ok();
    result.map(|()| (spec.name.to_string(), shown))
}

/// The command line entry point; returns whether every server passed.
pub fn run_cli(bundled_only: bool, only: Option<&str>) -> bool {
    let mut ok = true;
    let exe = std::env::current_exe().map(|e| e.display().to_string()).unwrap_or_default();
    println!("edittr {} at {exe}", env!("CARGO_PKG_VERSION"));
    let wanted = |s: &Sample| only.is_none_or(|o| o.split(',').any(|e| e.trim() == s.ext));
    for sample in SAMPLES.iter().filter(|s| wanted(s)) {
        let start = Instant::now();
        let result = round_trip(sample, bundled_only, Duration::from_secs(120));
        let secs = start.elapsed().as_secs_f32();
        match result {
            Ok((name, shown)) => println!("PASS {:<11} {name:<20} {secs:5.1}s  {shown}", sample.language),
            Err(e) => {
                ok = false;
                println!("FAIL {:<11} {e} ({secs:.1}s)", sample.language);
            }
        }
    }
    ok
}

#[cfg(test)]
pub fn assert_round_trip(ext: &str) {
    if let Err(e) = round_trip(sample(ext), false, Duration::from_secs(90)) {
        panic!("{e}");
    }
}
