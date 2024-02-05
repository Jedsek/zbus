use snakecase::ascii::to_snakecase;
use std::{
    fmt::{Display, Formatter, Write},
    process::{Command, Stdio},
};

use zbus::names::BusName;
use zbus_xml::{Arg, ArgDirection, Interface};
use zvariant::{
    Basic, CompleteType, ObjectPath, Signature, ARRAY_SIGNATURE_CHAR, DICT_ENTRY_SIG_END_CHAR,
    DICT_ENTRY_SIG_START_CHAR, STRUCT_SIG_END_CHAR, STRUCT_SIG_START_CHAR, VARIANT_SIGNATURE_CHAR,
};

pub struct GenTrait<'i> {
    pub interface: &'i Interface<'i>,
    pub service: Option<&'i BusName<'i>>,
    pub path: Option<&'i ObjectPath<'i>>,
}

impl<'i> Display for GenTrait<'i> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut unformatted = String::new();
        self.write_interface(&mut unformatted)?;

        let formatted = match format_generated_code(&unformatted) {
            Ok(formatted) => formatted,
            Err(e) => {
                eprintln!("Failed to format generated code: {}", e);
                unformatted
            }
        };

        write!(f, "{}", formatted)
    }
}

impl<'i> GenTrait<'i> {
    fn write_interface<W: Write>(&self, w: &mut W) -> std::fmt::Result {
        let iface = self.interface;
        let idx = iface.name().rfind('.').unwrap() + 1;
        let name = &iface.name()[idx..];

        write!(w, "#[proxy(interface = \"{}\"", iface.name())?;
        if let Some(service) = self.service {
            write!(w, ", default_service = \"{service}\"")?;
        }
        if let Some(path) = self.path {
            write!(w, ", default_path = \"{path}\"")?;
        }
        if self.path.is_none() || self.service.is_none() {
            write!(w, ", assume_defaults = true")?;
        }
        writeln!(w, ")]")?;
        writeln!(w, "trait {name} {{")?;

        let mut methods = iface.methods().to_vec();
        methods.sort_by(|a, b| a.name().partial_cmp(&b.name()).unwrap());
        for m in &methods {
            let (inputs, output) = inputs_output_from_args(m.args());
            let name = to_identifier(&to_snakecase(m.name().as_str()));
            writeln!(w)?;
            writeln!(w, "    /// {} method", m.name())?;
            if pascal_case(&name) != m.name().as_str() {
                writeln!(w, "    #[zbus(name = \"{}\")]", m.name())?;
            }
            hide_clippy_lints(w, m)?;
            writeln!(w, "    fn {name}({inputs}){output};")?;
        }

        let mut signals = iface.signals().to_vec();
        signals.sort_by(|a, b| a.name().partial_cmp(&b.name()).unwrap());
        for signal in &signals {
            let args = parse_signal_args(signal.args());
            let name = to_identifier(&to_snakecase(signal.name().as_str()));
            writeln!(w)?;
            writeln!(w, "    /// {} signal", signal.name())?;
            if pascal_case(&name) != signal.name().as_str() {
                writeln!(w, "    #[zbus(signal, name = \"{}\")]", signal.name())?;
            } else {
                writeln!(w, "    #[zbus(signal)]")?;
            }
            writeln!(w, "    fn {name}({args}) -> zbus::Result<()>;",)?;
        }

        let mut props = iface.properties().to_vec();
        props.sort_by(|a, b| a.name().partial_cmp(&b.name()).unwrap());
        for p in props {
            let name = to_identifier(&to_snakecase(p.name().as_str()));
            let fn_attribute = if pascal_case(&name) != p.name().as_str() {
                format!("    #[zbus(property, name = \"{}\")]", p.name())
            } else {
                "    #[zbus(property)]".to_string()
            };

            writeln!(w)?;
            writeln!(w, "    /// {} property", p.name())?;
            if p.access().read() {
                writeln!(w, "{}", fn_attribute)?;
                let output = to_rust_type(p.ty(), false, false);
                hide_clippy_type_complexity_lint(w, p.ty().signature())?;
                writeln!(w, "    fn {name}(&self) -> zbus::Result<{output}>;",)?;
            }

            if p.access().write() {
                writeln!(w, "{}", fn_attribute)?;
                let input = to_rust_type(p.ty(), true, true);
                writeln!(
                    w,
                    "    fn set_{name}(&self, value: {input}) -> zbus::Result<()>;",
                )?;
            }
        }
        writeln!(w, "}}")
    }
}

fn hide_clippy_lints<W: Write>(write: &mut W, method: &zbus_xml::Method<'_>) -> std::fmt::Result {
    // check for <https://rust-lang.github.io/rust-clippy/master/index.html#/too_many_arguments>
    // triggers when a functions has at least 7 paramters
    if method.args().len() >= 7 {
        writeln!(write, "    #[allow(clippy::too_many_arguments)]")?;
    }

    // check for <https://rust-lang.github.io/rust-clippy/master/index.html#/type_complexity>
    for arg in method.args() {
        let signature = arg.ty().signature();
        hide_clippy_type_complexity_lint(write, signature)?;
    }

    Ok(())
}

fn hide_clippy_type_complexity_lint<W: Write>(
    write: &mut W,
    signature: &zvariant::Signature,
) -> std::fmt::Result {
    let mut it = signature.as_bytes().iter().peekable();
    let complexity = estimate_type_complexity(&mut it);
    if complexity >= 1700 {
        writeln!(write, "    #[allow(clippy::type_complexity)]")?;
    }
    Ok(())
}

fn inputs_output_from_args(args: &[Arg]) -> (String, String) {
    let mut inputs = vec!["&self".to_string()];
    let mut output = vec![];
    let mut n = 0;
    let mut gen_name = || {
        n += 1;
        format!("arg_{n}")
    };

    for a in args {
        match a.direction() {
            None | Some(ArgDirection::In) => {
                let ty = to_rust_type(a.ty(), true, true);
                let arg = if let Some(name) = a.name() {
                    to_identifier(name)
                } else {
                    gen_name()
                };
                inputs.push(format!("{arg}: {ty}"));
            }
            Some(ArgDirection::Out) => {
                let ty = to_rust_type(a.ty(), false, false);
                output.push(ty);
            }
        }
    }

    let output = match output.len() {
        0 => "()".to_string(),
        1 => output[0].to_string(),
        _ => format!("({})", output.join(", ")),
    };

    (inputs.join(", "), format!(" -> zbus::Result<{output}>"))
}

fn parse_signal_args(args: &[Arg]) -> String {
    let mut inputs = vec!["&self".to_string()];
    let mut n = 0;
    let mut gen_name = || {
        n += 1;
        format!("arg_{n}")
    };

    for a in args {
        let ty = to_rust_type(a.ty(), true, false);
        let arg = if let Some(name) = a.name() {
            to_identifier(name)
        } else {
            gen_name()
        };
        inputs.push(format!("{arg}: {ty}"));
    }

    inputs.join(", ")
}

fn to_rust_type(ty: &CompleteType, input: bool, as_ref: bool) -> String {
    // can't haz recursive closure, yet
    fn iter_to_rust_type(
        it: &mut std::iter::Peekable<std::slice::Iter<'_, u8>>,
        input: bool,
        as_ref: bool,
    ) -> String {
        let c = it.next().unwrap();
        match *c as char {
            u8::SIGNATURE_CHAR => "u8".into(),
            bool::SIGNATURE_CHAR => "bool".into(),
            i16::SIGNATURE_CHAR => "i16".into(),
            u16::SIGNATURE_CHAR => "u16".into(),
            i32::SIGNATURE_CHAR => "i32".into(),
            u32::SIGNATURE_CHAR => "u32".into(),
            i64::SIGNATURE_CHAR => "i64".into(),
            u64::SIGNATURE_CHAR => "u64".into(),
            f64::SIGNATURE_CHAR => "f64".into(),
            // xmlgen accepts 'h' on Windows, only for code generation
            'h' => (if input {
                "zbus::zvariant::Fd<'_>"
            } else {
                "zbus::zvariant::OwnedFd"
            })
            .into(),
            <&str>::SIGNATURE_CHAR => (if input || as_ref { "&str" } else { "String" }).into(),
            ObjectPath::SIGNATURE_CHAR => (if input {
                if as_ref {
                    "&zbus::zvariant::ObjectPath<'_>"
                } else {
                    "zbus::zvariant::ObjectPath<'_>"
                }
            } else {
                "zbus::zvariant::OwnedObjectPath"
            })
            .into(),
            Signature::SIGNATURE_CHAR => (if input {
                if as_ref {
                    "&zbus::zvariant::Signature<'_>"
                } else {
                    "zbus::zvariant::Signature<'_>"
                }
            } else {
                "zbus::zvariant::OwnedSignature"
            })
            .into(),
            VARIANT_SIGNATURE_CHAR => (if input {
                if as_ref {
                    "&zbus::zvariant::Value<'_>"
                } else {
                    "zbus::zvariant::Value<'_>"
                }
            } else {
                "zbus::zvariant::OwnedValue"
            })
            .into(),
            ARRAY_SIGNATURE_CHAR => {
                let c = it.peek().unwrap();
                match **c as char {
                    '{' => format!(
                        "std::collections::HashMap<{}>",
                        iter_to_rust_type(it, input, false)
                    ),
                    _ => {
                        let ty = iter_to_rust_type(it, input, false);
                        if input {
                            format!("&[{ty}]")
                        } else {
                            format!("{}Vec<{}>", if as_ref { "&" } else { "" }, ty)
                        }
                    }
                }
            }
            c @ STRUCT_SIG_START_CHAR | c @ DICT_ENTRY_SIG_START_CHAR => {
                let dict = c == '{';
                let mut vec = vec![];
                loop {
                    let c = it.peek().unwrap();
                    match **c as char {
                        STRUCT_SIG_END_CHAR | DICT_ENTRY_SIG_END_CHAR => {
                            // consume the closing character
                            it.next().unwrap();
                            break;
                        }
                        _ => vec.push(iter_to_rust_type(it, input, false)),
                    }
                }
                if dict {
                    vec.join(", ")
                } else if vec.len() > 1 {
                    format!("{}({})", if as_ref { "&" } else { "" }, vec.join(", "))
                } else {
                    format!("{}({},)", if as_ref { "&" } else { "" }, vec[0])
                }
            }
            _ => unimplemented!(),
        }
    }

    let mut it = ty.signature().as_bytes().iter().peekable();
    iter_to_rust_type(&mut it, input, as_ref)
}

static KWORDS: &[&str] = &[
    "Self", "abstract", "as", "async", "await", "become", "box", "break", "const", "continue",
    "crate", "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "typeof",
    "union", "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

fn to_identifier(id: &str) -> String {
    if KWORDS.contains(&id) {
        format!("{id}_")
    } else {
        id.replace('-', "_")
    }
}

// This function is the same as zbus_macros::utils::pascal_case
pub fn pascal_case(s: &str) -> String {
    let mut pascal = String::new();
    let mut capitalize = true;
    for ch in s.chars() {
        if ch == '_' {
            capitalize = true;
        } else if capitalize {
            pascal.push(ch.to_ascii_uppercase());
            capitalize = false;
        } else {
            pascal.push(ch);
        }
    }
    pascal
}

fn estimate_type_complexity(it: &mut std::iter::Peekable<std::slice::Iter<'_, u8>>) -> u32 {
    let mut score = 0;
    let c = it.next().unwrap();
    match *c as char {
        u8::SIGNATURE_CHAR
        | bool::SIGNATURE_CHAR
        | i16::SIGNATURE_CHAR
        | u16::SIGNATURE_CHAR
        | i32::SIGNATURE_CHAR
        | u32::SIGNATURE_CHAR
        | i64::SIGNATURE_CHAR
        | u64::SIGNATURE_CHAR
        | f64::SIGNATURE_CHAR
        | <&str>::SIGNATURE_CHAR => {
            score += 1;
        }
        'h' => score += 10,
        Signature::SIGNATURE_CHAR | VARIANT_SIGNATURE_CHAR | ObjectPath::SIGNATURE_CHAR => {
            score *= 10
        }
        ARRAY_SIGNATURE_CHAR => {
            let c = it.peek().unwrap();
            match **c as char {
                '{' => {
                    score *= 10;
                    score += estimate_type_complexity(it);
                }
                _ => {
                    score += 5 * estimate_type_complexity(it);
                }
            }
        }
        STRUCT_SIG_START_CHAR | DICT_ENTRY_SIG_START_CHAR => {
            score += 50;
            loop {
                let c = it.peek().unwrap();
                match **c as char {
                    STRUCT_SIG_END_CHAR | DICT_ENTRY_SIG_END_CHAR => {
                        // consume the closing character
                        it.next().unwrap();
                        break;
                    }
                    _ => score += 5 * estimate_type_complexity(it),
                }
            }
        }
        _ => {}
    };
    score
}

fn format_generated_code(generated_code: &str) -> std::io::Result<String> {
    use std::io::{Read, Write};

    let mut process = Command::new("rustfmt")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // rustfmt may post warnings about features not being enabled on stable rust
        // these can be distracting and are irrevelant to the user, so we hide them
        .stderr(Stdio::null())
        .spawn()?;
    let rustfmt_stdin = process.stdin.as_mut().unwrap();
    let mut rustfmt_stdout = process.stdout.take().unwrap();
    writeln!(rustfmt_stdin)?;
    rustfmt_stdin.write_all(generated_code.as_bytes())?;

    process.wait()?;
    let mut formatted = String::new();
    rustfmt_stdout.read_to_string(&mut formatted)?;

    Ok(formatted)
}
