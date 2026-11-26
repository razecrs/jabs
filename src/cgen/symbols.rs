//! A consistency check over a program's generated C: every JABS symbol the code refers to
//! (class records, methods, stubs, initializers, static fields, vtables, interface tables and
//! lists) must be defined somewhere in the program. A miss would otherwise surface only as a C
//! compiler error or a linker error in a build of thousands of files, far from its cause; here
//! the compilation stops with the symbol and the definition that refers to it.
//!
//! The check reads the text the emitters write, which has a fixed shape: a definition starts in
//! column 0 (`const jabs_class jk_X = {`, `T jm_X(...) {`, `T jf_X = ...;`,
//! `void (*const jv_X[])(void) = {`), declarations are `extern` lines or column-0 prototypes
//! ending in `;`, and everything else that names a symbol refers to it.

use std::collections::{BTreeMap, HashSet};

/// Prefixes of the symbols JABS generates (cgen.rs `mangled`). `jo_` names struct types, which
/// the C compiler checks itself.
const PREFIXES: [&str; 11] = [
    "jk_", "jm_", "jc_", "jf_", "jv_", "ji_", "jn_", "jx_", "jxe_", "jcl_", "jcm_",
];
/// Undefined symbols listed in one failure; the first few are enough to find the cause.
const MAX_REPORTED: usize = 20;

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Readable form of a mangled symbol: `__` separates parts, `_XX` is a hex-escaped byte.
fn demangle(symbol: &str) -> String {
    let Some(at) = symbol.find('_') else {
        return symbol.to_string();
    };
    let bytes = &symbol.as_bytes()[at + 1..];
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_' && bytes.get(i + 1) == Some(&b'_') {
            out.push(' ');
            i += 2;
            continue;
        }
        if bytes[i] == b'_'
            && let Some(hex) = symbol.get(at + 2 + i..at + 4 + i)
            && let Ok(value) = u8::from_str_radix(hex, 16)
        {
            out.push(char::from(value));
            i += 3;
            continue;
        }
        out.push(char::from(bytes[i]));
        i += 1;
    }
    out
}

/// What a column-0 line does with its first JABS symbol, judged by what follows the name.
enum FirstUse {
    Defines,
    Declares,
    Refers,
}

fn first_use(after: &str, line: &str) -> FirstUse {
    let trimmed = line.trim_end();
    if after.starts_with('(') {
        // A function: a body on this line or opening here defines it; `;` only declares it.
        if trimmed.ends_with('{') || line.contains(") {") {
            return FirstUse::Defines;
        }
        if trimmed.ends_with(';') {
            return FirstUse::Declares;
        }
        return FirstUse::Refers;
    }
    if after.starts_with(" =") || after.starts_with('=') || after.starts_with(';') {
        return FirstUse::Defines;
    }
    if after.starts_with('[')
        && (trimmed.ends_with('{') || trimmed.contains("] =") || trimmed.contains("])(void) ="))
    {
        return FirstUse::Defines;
    }
    if after.starts_with('[') && trimmed.ends_with(';') {
        return FirstUse::Defines;
    }
    FirstUse::Refers
}

/// Check that every JABS symbol referred to in `files` (name, text) is defined in one of them.
/// Err lists the undefined symbols, each with the definition (and file) that refers to it.
pub fn check(files: &[(String, String)]) -> Result<(), String> {
    let mut defined: HashSet<&str> = HashSet::new();
    // symbol -> (file, line number, enclosing definition) of its first reference
    let mut referred: BTreeMap<&str, (&str, usize, &str)> = BTreeMap::new();
    for (file, text) in files {
        let mut enclosing: &str = "(file scope)";
        for (number, line) in text.lines().enumerate() {
            let skip = line.starts_with('#')
                || line.starts_with("extern ")
                || line.starts_with("typedef")
                || line.starts_with("/*")
                || line.starts_with("//")
                || line.starts_with(" *");
            if skip {
                continue;
            }
            let column0 = !line.starts_with([' ', '\t', '{', '}']);
            let bytes = line.as_bytes();
            let mut first = true;
            let mut i = 0;
            while i < bytes.len() {
                if !ident_byte(bytes[i]) || (i > 0 && ident_byte(bytes[i - 1])) {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < bytes.len() && ident_byte(bytes[i]) {
                    i += 1;
                }
                let ident = &line[start..i];
                if !PREFIXES.iter().any(|p| ident.starts_with(p)) {
                    continue;
                }
                let is_first = first;
                first = false;
                if column0 && is_first {
                    match first_use(&line[i..], line) {
                        FirstUse::Defines => {
                            defined.insert(ident);
                            enclosing = ident;
                            continue;
                        }
                        FirstUse::Declares => continue,
                        FirstUse::Refers => {}
                    }
                }
                referred
                    .entry(ident)
                    .or_insert((file.as_str(), number + 1, enclosing));
            }
        }
    }
    let missing: Vec<(&&str, &(&str, usize, &str))> = referred
        .iter()
        .filter(|(symbol, _)| !defined.contains(**symbol))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut message = format!(
        "generated C refers to {} JABS symbol(s) that nothing defines (an emitter names a record, \
         method, stub or initializer that emission left out):\n",
        missing.len()
    );
    for (symbol, (file, line, enclosing)) in missing.iter().take(MAX_REPORTED) {
        message.push_str(&format!(
            "  {symbol} ({}) referred to by {enclosing} ({}) at {file}:{line}\n",
            demangle(symbol),
            demangle(enclosing)
        ));
    }
    Err(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(text: &str) -> Vec<(String, String)> {
        vec![("unit_1.c".to_string(), text.to_string())]
    }

    #[test]
    fn defined_and_declared_symbols_pass() {
        let text = "extern const jabs_class jk_A;\n\
                    const jabs_class jk_A = { \"A\" };\n\
                    int32_t jm_A__f(jabs_object *self);\n\
                    int32_t jm_A__f(jabs_object *self) {\n    return jm_A__g(self) + (int32_t)(intptr_t)&jk_A;\n}\n\
                    int32_t jm_A__g(jabs_object *self) { (void)self; return jf_A__x; }\n\
                    int32_t jf_A__x = 0;\n\
                    void (*const jv_A[])(void) = { (void (*)(void))jm_A__f };\n";
        assert_eq!(check(&files(text)), Ok(()));
    }

    #[test]
    fn a_reference_without_a_definition_names_it_and_its_referrer() {
        let text = "int32_t jm_A__f(void);\n\
                    int jc_A__init(void) {\n    jm_A__f();\n    return 0;\n}\n";
        let error = check(&files(text)).unwrap_err();
        assert!(error.contains("jm_A__f"), "{error}");
        assert!(error.contains("referred to by jc_A__init"), "{error}");
        assert!(error.contains("unit_1.c:3"), "{error}");
    }

    #[test]
    fn a_record_named_by_an_allocation_must_be_emitted() {
        // The bug this check was written for: multianewarray named the record of an element
        // class that RTA had not recorded.
        let text =
            "void jm_F__init(void) {\n    jabs_multi_new_array(\"[[[LD;\", 2, counts, &jk_D);\n}\n";
        let error = check(&files(text)).unwrap_err();
        assert!(error.contains("jk_D"), "{error}");
    }
}
