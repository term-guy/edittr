//! C/C++ compiler families, so clangd can work with projects built by embedded and
//! automotive toolchains it can't run or understand (IAR, TI, Keil, TASKING, Green
//! Hills, Wind River Diab, Renesas, Cosmic, SDCC, CodeWarrior).
//!
//! GCC- and Clang-compatible compilers (including cross GCCs such as
//! arm-none-eabi-gcc, xc32-gcc, tricore-gcc, and armclang / tiarmclang) are left
//! alone: clangd understands their flags, and `--query-driver` gives it their
//! system headers. For the others, `translate` rewrites a compile command into a
//! clang one: include paths, defines and pre-includes are kept (converted from the
//! compiler's own spelling), everything else is dropped, and the compiler's
//! predefined macros, keyword extensions (`__no_init`, `__interrupt`, ...) and
//! system headers (found from its install folder) are added.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    Gcc,
    Clang,
    Msvc,
    Iar,
    Ti,
    Armcc,
    KeilC51,
    Tasking,
    GreenHills,
    Diab,
    Renesas,
    Cosmic,
    Sdcc,
    CodeWarrior,
}

impl Family {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Gcc => "GCC",
            Self::Clang => "Clang",
            Self::Msvc => "MSVC",
            Self::Iar => "IAR",
            Self::Ti => "TI",
            Self::Armcc => "Arm Compiler 5 (Keil)",
            Self::KeilC51 => "Keil C51/C166",
            Self::Tasking => "TASKING",
            Self::GreenHills => "Green Hills",
            Self::Diab => "Wind River Diab",
            Self::Renesas => "Renesas",
            Self::Cosmic => "Cosmic",
            Self::Sdcc => "SDCC",
            Self::CodeWarrior => "CodeWarrior",
        }
    }

    /// Whether clangd understands this compiler's command lines as they are.
    pub(crate) fn native(self) -> bool {
        matches!(self, Self::Gcc | Self::Clang | Self::Msvc)
    }
}

/// The compiler's file name without directory, `.exe` and a numeric version suffix.
fn base_name(word: &str) -> String {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    let name = name.strip_suffix(".exe").or_else(|| name.strip_suffix(".EXE")).unwrap_or(name);
    let name = name
        .rsplit_once('-')
        .filter(|(_, v)| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map_or(name, |(b, _)| b);
    name.to_ascii_lowercase()
}

/// The family of a compiler command word (`/opt/iar/arm/bin/iccarm`, `armcl.exe`, `gcc-13`).
pub(crate) fn family(word: &str) -> Option<Family> {
    let name = base_name(word);
    let n = name.as_str();
    let f = match n {
        "cc" | "c++" | "gcc" | "g++" | "icc" | "icx" | "icpx" | "tcc" | "xc8-cc" => Family::Gcc,
        "clang" | "clang++" | "armclang" | "tiarmclang" | "ac6" => Family::Clang,
        "cl" | "clang-cl" => Family::Msvc,
        "armcl" | "cl430" | "cl2000" | "cl6x" | "cl7x" | "clpru" | "cla" => Family::Ti,
        "armcc" | "tcc_arm" => Family::Armcc,
        "c51" | "cx51" | "c166" | "c251" => Family::KeilC51,
        "ctc" | "cctc" | "carm" | "ccarm_tasking" | "cptc" | "cmcs" | "c563" | "cc51" => Family::Tasking,
        "dcc" | "dplus" => Family::Diab,
        "ccrh" | "ccrl" | "ccrx" => Family::Renesas,
        "cxstm8" | "cx6812" | "cxs12x" | "cx6808" => Family::Cosmic,
        "sdcc" => Family::Sdcc,
        _ if n.starts_with("mwcc") => Family::CodeWarrior,
        _ if n.starts_with("icc") && n.len() > 3 => Family::Iar,
        // Green Hills: ccarm, cxarm, ccppc, cxppc, ccrh850, ccv850, ccintarm, ...
        _ if (n.starts_with("cc") || n.starts_with("cx"))
            && [
                "arm", "ppc", "rh850", "v850", "thumb", "intarm", "intppc", "intrh850", "riscv", "tricore",
            ]
            .iter()
            .any(|t| &n[2..] == *t) =>
        {
            Family::GreenHills
        }
        // Cross GCC / Clang: arm-none-eabi-gcc, xtensa-esp32-elf-g++, avr-gcc, xc32-gcc, tricore-gcc.
        _ if ["-gcc", "-g++", "-cc", "-c++"].iter().any(|s| n.ends_with(s)) => Family::Gcc,
        _ if n.ends_with("-clang") || n.ends_with("-clang++") => Family::Clang,
        _ => return None,
    };
    Some(f)
}

/// Whether a command word is a compiler; for the non-native families a single source
/// argument means a compile even without `-c` (IAR and others compile by default).
pub(crate) fn is_compiler(word: &str) -> bool {
    family(word).is_some()
}

struct Profile {
    target: Option<&'static str>,
    macros: Vec<String>,
    cxx: bool,
}

fn profile(family: Family, compiler: &str, args: &[String]) -> Profile {
    let name = base_name(compiler);
    let mut cxx = false;
    let m = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (target, macros) = match family {
        Family::Iar => {
            let variant = name.trim_start_matches("icc").to_ascii_uppercase();
            let target = match variant.as_str() {
                "ARM" => Some("arm-none-eabi"),
                "RISCV" => Some("riscv32-unknown-elf"),
                "430" => Some("msp430"),
                "AVR" => Some("avr"),
                _ => None,
            };
            let mut macros = m(&[
                "__IAR_SYSTEMS_ICC__=9",
                "__no_init=",
                "__root=",
                "__ramfunc=",
                "__interrupt=",
                "__task=",
                "__monitor=",
                "__nested=",
                "__intrinsic=",
                "__nounwind=",
                "__irq=",
                "__fiq=",
                "__swi=",
                "__arm=",
                "__thumb=",
                "__stackless=",
                "__weak=__attribute__((weak))",
                "__packed=__attribute__((packed))",
                "__noreturn=__attribute__((noreturn))",
                "__ALIGNOF__=__alignof__",
                "__section_begin(x)=((void *)0)",
                "__section_end(x)=((void *)0)",
                "__section_size(x)=0",
                "__near=",
                "__far=",
                "__huge=",
                "__tiny=",
                "__data16=",
                "__data20=",
                "__data24=",
                "__code=",
                "__xdata=",
                "__idata=",
                "__pdata=",
                "__sfr=",
                "__saddr=",
                "__callt=",
            ]);
            macros.push(format!("__ICC{variant}__=1"));
            (target, macros)
        }
        Family::Ti => {
            let (target, own) = match name.as_str() {
                "armcl" => (Some("arm-none-eabi"), "__TI_ARM__=1"),
                "cl430" => (Some("msp430"), "__MSP430__=1"),
                "cl2000" | "cla" => (None, "__TMS320C2000__=1"),
                "cl6x" | "cl7x" => (None, "_TMS320C6X=1"),
                _ => (None, "__TI_PRU__=1"),
            };
            let mut macros = m(&[
                "__TI_COMPILER_VERSION__=21006000",
                "__interrupt=",
                "interrupt=",
                "__cregister=",
                "cregister=",
                "__near=",
                "__far=",
                "__asm=__asm__",
            ]);
            macros.push(own.into());
            (target, macros)
        }
        Family::Armcc => (
            Some("arm-none-eabi"),
            m(&[
                "__CC_ARM=1",
                "__ARMCC_VERSION=5060960",
                "__irq=",
                "__svc(x)=",
                "__svc_indirect(x)=",
                "__value_in_regs=",
                "__softfp=",
                "__pure=",
                "__smc(x)=",
                "__forceinline=inline",
                "__inline=inline",
                "__weak=__attribute__((weak))",
                "__packed=__attribute__((packed))",
                "__align(x)=__attribute__((aligned(x)))",
                "__asm=__asm__",
            ]),
        ),
        Family::KeilC51 => {
            let own = if name == "c166" { "__C166__=1" } else { "__C51__=1" };
            (
                None,
                vec![
                    own.into(),
                    "_at_=".into(),
                    "reentrant=".into(),
                    "small=".into(),
                    "large=".into(),
                    "compact=".into(),
                ],
            )
        }
        Family::Tasking => {
            let (target, own): (Option<&str>, &[&str]) = match name.as_str() {
                "ctc" | "cctc" => (None, &["__CTC__=1", "__TRICORE__=1"]),
                "carm" => (Some("arm-none-eabi"), &["__CARM__=1"]),
                "cptc" => (None, &["__CPTC__=1"]),
                _ => (None, &[]),
            };
            let mut macros = m(&[
                "__TASKING__=1",
                "__interrupt(x)=",
                "__vector_table(x)=",
                "__noinline=__attribute__((noinline))",
                "__near=",
                "__far=",
                "__a0=",
                "__a1=",
                "__a8=",
                "__a9=",
                "__share=",
                "__private0=",
                "__private1=",
                "__private2=",
                "__fract=float",
                "__sfract=float",
                "__laccum=float",
                "__packed__=__attribute__((packed))",
            ]);
            macros.extend(m(own));
            (target, macros)
        }
        Family::GreenHills => {
            cxx = name.starts_with("cx");
            let target = if name.ends_with("arm") || name.ends_with("thumb") {
                Some("arm-none-eabi")
            } else if name.ends_with("ppc") {
                Some("powerpc-unknown-eabi")
            } else if name.ends_with("riscv") {
                Some("riscv32-unknown-elf")
            } else {
                None
            };
            (
                target,
                m(&[
                    "__ghs__=1",
                    "__GHS_VERSION_NUMBER=202214",
                    "__interrupt=",
                    "__packed=__attribute__((packed))",
                    "__inline=inline",
                ]),
            )
        }
        Family::Diab => {
            cxx = name == "dplus";
            let t = args.iter().find_map(|a| a.strip_prefix("-t")).unwrap_or("").to_ascii_uppercase();
            let target = if t.starts_with("ARM") {
                Some("arm-none-eabi")
            } else if t.starts_with("PPC") || t.is_empty() {
                Some("powerpc-unknown-eabi")
            } else {
                None
            };
            (
                target,
                m(&["__DCC__=1", "__diab=1", "__interrupt__=", "__asm=__asm__", "__inline__=inline"]),
            )
        }
        Family::Renesas => {
            let own: &[&str] = match name.as_str() {
                "ccrh" => &["__CCRH__=1", "__RH850__=1", "__v850e3v5__=1"],
                "ccrl" => &["__CCRL__=1", "__RL78__=1"],
                _ => &["__CCRX__=1", "__RX=1", "__RX__=1"],
            };
            let mut macros = m(&[
                "__interrupt=",
                "__near=",
                "__far=",
                "__saddr=",
                "__callt=",
                "__evenaccess=",
                "__sectop(x)=((void *)0)",
                "__secend(x)=((void *)0)",
            ]);
            macros.extend(m(own));
            (None, macros)
        }
        Family::Cosmic => (None, m(&["__CSMC__=1"])),
        Family::Sdcc => (
            None,
            m(&[
                "__SDCC=1",
                "__sfr=volatile unsigned char",
                "__sfr16=volatile unsigned short",
                "__sbit=volatile _Bool",
                "__bit=_Bool",
                "__data=",
                "__idata=",
                "__xdata=",
                "__pdata=",
                "__code=",
                "__near=",
                "__far=",
                "__at(x)=",
                "__interrupt(x)=",
                "__using(x)=",
                "__critical=",
                "__naked=",
                "__reentrant=",
                "__banked=",
            ]),
        ),
        Family::CodeWarrior => {
            let target = if name.contains("ppc") {
                Some("powerpc-unknown-eabi")
            } else if name.contains("arm") {
                Some("arm-none-eabi")
            } else {
                None
            };
            (target, m(&["__MWERKS__=1", "__CWCC__=1", "__declspec(x)="]))
        }
        Family::Gcc | Family::Clang | Family::Msvc => (None, Vec::new()),
    };
    Profile { target, macros, cxx }
}

/// System header folders of an installed non-native compiler, found from where it lives.
fn system_includes(family: Family, compiler: &str, cxx: bool) -> Vec<PathBuf> {
    let Some(exe) = resolve(compiler) else { return Vec::new() };
    let Some(bin) = exe.parent() else { return Vec::new() };
    let Some(top) = bin.parent() else { return Vec::new() };
    let candidates: Vec<PathBuf> = match family {
        // IAR: <install>/arm/bin/iccarm -> <install>/arm/inc/c (+ inc/cpp for C++), inc.
        Family::Iar => {
            let mut v = Vec::new();
            if cxx {
                v.push(top.join("inc/cpp"));
            }
            v.extend([top.join("inc/c"), top.join("inc")]);
            v
        }
        // Keil MDK: ARM/ARMCC/bin/armcc -> ARM/ARMCC/include; C51/BIN/C51 -> C51/INC.
        Family::KeilC51 => vec![top.join("INC"), top.join("inc")],
        Family::Tasking => vec![top.join("include"), top.join("include.c")],
        // Green Hills: the compiler sits in the install root.
        Family::GreenHills => vec![
            bin.join("ansi"),
            bin.join("include/arm"),
            bin.join("include/ppc"),
            bin.join("include/v800"),
        ],
        Family::Diab => vec![top.join("include")],
        Family::Renesas => vec![top.join("inc"), top.join("include")],
        Family::Cosmic => vec![bin.join("Hstm8"), bin.join("H6812"), bin.join("Hs12x"), top.join("include")],
        Family::Sdcc => vec![top.join("share/sdcc/include"), top.join("include")],
        _ => vec![top.join("include")],
    };
    candidates.into_iter().filter(|p| p.is_dir()).collect()
}

fn resolve(compiler: &str) -> Option<PathBuf> {
    let path = Path::new(compiler);
    if path.is_absolute() && path.is_file() {
        return Some(path.to_owned());
    }
    if path.components().count() > 1 {
        return path.canonicalize().ok();
    }
    crate::lsp::which(compiler)
}

const CXX_EXTS: &[&str] = &["cc", "cpp", "cxx", "c++", "mm", "hpp", "hh"];

/// Rewrite a non-native compile command as a clang one (see the module docs);
/// `None` when clangd can use the command as it is.
pub(crate) fn translate(args: &[String], file: &str) -> Option<Vec<String>> {
    let start = args.iter().position(|a| is_compiler(a))?;
    let compiler = &args[start];
    let family = family(compiler)?;
    if family.native() {
        return None;
    }
    let args = &args[start + 1..];
    let profile = profile(family, compiler, args);
    let mut cxx = profile.cxx
        || Path::new(file)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| CXX_EXTS.contains(&e));
    let mut kept: Vec<String> = Vec::new();
    let mut std = None;
    let mut i = 0;
    // Takes the flag's value: `-Ifoo`, `-I foo`, `--opt=foo`, `--opt foo`.
    let value = |i: &mut usize, flag: &str, a: &str| -> Option<String> {
        if let Some(v) = a.strip_prefix(flag) {
            if let Some(v) = v.strip_prefix('=') {
                return Some(v.to_owned());
            }
            if !v.is_empty() {
                return Some(v.to_owned());
            }
            *i += 1;
            return args.get(*i).cloned();
        }
        None
    };
    while i < args.len() {
        let a = args[i].as_str();
        let lower = a.to_ascii_lowercase();
        if a == file || Path::new(a).file_name() == Path::new(file).file_name() && !a.starts_with('-') {
            i += 1;
            continue;
        }
        // Keil C51 / C166: INCDIR(a;b) DEFINE(X,Y=1)
        if family == Family::KeilC51 {
            let upper = a.to_ascii_uppercase();
            for (key, flag) in [("INCDIR", "-I"), ("DEFINE", "-D"), ("DF", "-D")] {
                if let Some(rest) = upper.strip_prefix(key) {
                    let rest = &a[a.len() - rest.len()..];
                    let inner = rest.trim().trim_start_matches('(').trim_end_matches(')');
                    for item in inner.split([';', ',']).map(str::trim).filter(|s| !s.is_empty()) {
                        kept.push(format!("{flag}{item}"));
                    }
                }
            }
            i += 1;
            continue;
        }
        let include_flags: &[&str] = match family {
            Family::Ti => &["--include_path", "-I", "-i"],
            Family::Cosmic => &["-i", "-I"],
            Family::Renesas => &["-include", "-I"],
            _ => &["-I"],
        };
        let define_flags: &[&str] = match family {
            Family::Ti => &["--define", "-D", "-d"],
            Family::Cosmic => &["-d", "-D"],
            Family::Renesas => &["-define", "-D"],
            _ => &["-D"],
        };
        let preinclude_flags: &[&str] = match family {
            Family::Iar | Family::Armcc | Family::Ti => &["--preinclude"],
            Family::Renesas => &["-Xpreinclude", "-preinclude"],
            Family::Tasking => &["--include-file", "-H"],
            Family::GreenHills => &["-preinclude", "-include"],
            Family::CodeWarrior => &["-prefix", "-include"],
            _ => &["-include"],
        };
        let mut matched = false;
        // Renesas CC-RX lists several values with commas: -include=a,b -define=X,Y=1
        let multi = family == Family::Renesas && (a.starts_with("-include=") || a.starts_with("-define="));
        for flag in include_flags {
            if let Some(v) = value(&mut i, flag, a) {
                for v in if multi {
                    v.split(',').map(str::to_owned).collect()
                } else {
                    vec![v]
                } {
                    kept.push(format!("-I{v}"));
                }
                matched = true;
                break;
            }
        }
        if !matched {
            for flag in define_flags {
                if let Some(v) = value(&mut i, flag, a) {
                    for v in if multi {
                        v.split(',').map(str::to_owned).collect()
                    } else {
                        vec![v]
                    } {
                        kept.push(format!("-D{v}"));
                    }
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            for flag in preinclude_flags {
                if let Some(v) = value(&mut i, flag, a) {
                    kept.extend(["-include".to_owned(), v]);
                    matched = true;
                    break;
                }
            }
        }
        if !matched {
            if let Some(v) = value(&mut i, "--system_include_dir", a) {
                kept.extend(["-isystem".to_owned(), v]);
            } else if let Some(v) = a.strip_prefix("-U") {
                kept.push(format!("-U{v}"));
            } else if lower == "--c++"
                || lower == "--ec++"
                || lower == "--eec++"
                || lower == "--cpp"
                || lower == "--cpp_default"
                || lower == "-xc++"
            {
                cxx = true;
            } else if lower == "--c89" || lower == "--strict_ansi" {
                std = Some("c89");
            } else if lower == "--c99" || lower == "--iso=99" {
                std = Some("c99");
            } else if lower == "--c11" || lower == "--iso=11" {
                std = Some("c11");
            } else if let Some(s) = a.strip_prefix("-std=").or_else(|| a.strip_prefix("--std=")) {
                std = None; // an explicit -std= wins over --c99 and friends
                kept.push(format!("-std={s}"));
            }
        }
        i += 1;
    }
    let mut out = vec![if cxx { "clang++".to_owned() } else { "clang".to_owned() }];
    if let Some(t) = profile.target {
        out.push(format!("--target={t}"));
    }
    if cxx && Path::new(file).extension().is_some_and(|e| e == "c") {
        out.push("-xc++".into());
    }
    if let Some(s) = std {
        out.push(format!("-std={s}"));
    }
    let system = system_includes(family, compiler, cxx);
    if !system.is_empty() {
        // The compiler's own C library replaces the host's.
        out.push("-nostdlibinc".into());
        for dir in system {
            out.extend(["-isystem".to_owned(), dir.display().to_string()]);
        }
    }
    out.extend([
        "-Wno-unknown-pragmas".to_owned(),
        "-Wno-unknown-attributes".to_owned(),
        "-Wno-ignored-attributes".to_owned(),
    ]);
    out.extend(profile.macros.into_iter().map(|m| format!("-D{m}")));
    out.extend(kept);
    out.extend(["-c".to_owned(), file.to_owned()]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        crate::compdb::split_words(s)
    }

    #[test]
    fn recognizes_families() {
        for (w, f) in [
            ("/opt/iar/arm/bin/iccarm.exe", Family::Iar),
            ("iccrx", Family::Iar),
            ("icc8051", Family::Iar),
            ("armcl", Family::Ti),
            ("cl430", Family::Ti),
            ("armcc", Family::Armcc),
            ("armclang", Family::Clang),
            ("C51.EXE", Family::KeilC51),
            ("ctc", Family::Tasking),
            ("ccarm", Family::GreenHills),
            ("cxppc", Family::GreenHills),
            ("ccrh850", Family::GreenHills),
            ("dcc", Family::Diab),
            ("ccrh", Family::Renesas),
            ("cxstm8", Family::Cosmic),
            ("sdcc", Family::Sdcc),
            ("mwcceppc", Family::CodeWarrior),
            ("arm-none-eabi-gcc", Family::Gcc),
            ("xc32-gcc", Family::Gcc),
            ("tricore-gcc", Family::Gcc),
            ("gcc-13", Family::Gcc),
            ("cl", Family::Msvc),
        ] {
            assert_eq!(family(w), Some(f), "{w}");
        }
        for w in ["ar", "ld", "make", "clang-format", "iccarm-config", "ccache", "echo"] {
            assert!(!is_compiler(w) || w == "iccarm-config" && family(w) == Some(Family::Iar), "{w}");
        }
    }

    #[test]
    fn gcc_and_clang_commands_are_left_alone() {
        assert_eq!(translate(&v("arm-none-eabi-gcc -mcpu=cortex-m4 -Iinc -c main.c"), "main.c"), None);
        assert_eq!(translate(&v("ccache clang -c main.c"), "main.c"), None);
    }

    #[test]
    fn translates_iar_ti_renesas_and_keil_c51() {
        let iar = translate(
            &v("iccarm main.c -e --cpu=Cortex-M4 -I inc -Iboard -DUSE_HAL=1 --preinclude cfg.h --dlib_config normal.h -o out/"),
            "main.c",
        )
        .unwrap();
        assert_eq!(iar[0], "clang");
        assert!(iar.contains(&"--target=arm-none-eabi".to_owned()));
        assert!(iar.contains(&"-D__ICCARM__=1".to_owned()) && iar.contains(&"-D__no_init=".to_owned()));
        assert_eq!(iar[iar.len() - 2..], ["-c", "main.c"]);
        assert!(
            iar.windows(5)
                .any(|w| w == ["-Iinc", "-Iboard", "-DUSE_HAL=1", "-include", "cfg.h"])
        );
        assert!(!iar.iter().any(|a| a.starts_with("--cpu") || a == "-e" || a.contains("dlib")));

        let ti = translate(
            &v("armcl -mv7M4 --include_path=\"C:/ti/inc\" --define=PART_TM4C --preinclude=x.h -c a.c"),
            "a.c",
        )
        .unwrap();
        assert!(ti.windows(4).any(|w| w == ["-IC:/ti/inc", "-DPART_TM4C", "-include", "x.h"]));
        assert!(ti.contains(&"-D__TI_ARM__=1".to_owned()));

        let rx = translate(&v("ccrx -cpu=rx600 -include=inc,drv -define=A=1,B -lang=c99 a.c"), "a.c").unwrap();
        assert!(rx.windows(4).any(|w| w == ["-Iinc", "-Idrv", "-DA=1", "-DB"]));

        let c51 = translate(&v("C51 main.c INCDIR(../inc;../drv) DEFINE(X,Y=2) OPTIMIZE(8)"), "main.c").unwrap();
        assert!(c51.windows(4).any(|w| w == ["-I../inc", "-I../drv", "-DX", "-DY=2"]));

        let ghs = translate(&v("cxarm -cpu=cortexm4 -Iinc -c a.c"), "a.c").unwrap();
        assert_eq!(&ghs[..3], ["clang++", "--target=arm-none-eabi", "-xc++"]);
    }

    #[test]
    fn finds_iar_system_headers_from_the_install_folder() {
        let top = std::env::temp_dir().join(format!("edittr-iar-{}", std::process::id())).join("arm");
        std::fs::create_dir_all(top.join("bin")).unwrap();
        std::fs::create_dir_all(top.join("inc/c")).unwrap();
        let exe = top.join("bin/iccarm");
        std::fs::write(&exe, "").unwrap();
        let out = translate(&[exe.display().to_string(), "-c".into(), "a.c".into()], "a.c").unwrap();
        let inc = top.join("inc/c").display().to_string();
        assert!(out.windows(3).any(|w| w == ["-nostdlibinc", "-isystem", inc.as_str()]), "{out:?}");
        std::fs::remove_dir_all(top.parent().unwrap()).ok();
    }
}
