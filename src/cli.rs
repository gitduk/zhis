//! Minimal Go-style flag parsing, and the two ways a command dies: a usage
//! error (exit 2) or a store error (exit 1).

use std::collections::HashMap;
use std::process::exit;

use crate::store::StoreError;

pub struct Flags {
    values: HashMap<String, String>,
    pub positionals: Vec<String>,
}

impl Flags {
    pub fn get(&self, name: &str) -> Option<&String> {
        self.values.get(name)
    }
    pub fn has(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }
}

/// Parses `-name value`, `-name=value`, and `--name` forms. Bool flags
/// (`bools`) never consume the next argument. Unknown flags are rejected.
fn parse_flags(args: &[String], known: &[&str], bools: &[&str]) -> Result<Flags, String> {
    let mut values = HashMap::new();
    let mut positionals = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let rest = strip_dashes(arg);
        match rest {
            None => {
                positionals.push(arg.clone());
                i += 1;
            }
            Some(rest) => {
                let (name, inline) = match rest.split_once('=') {
                    Some((n, v)) => (n.to_string(), Some(v.to_string())),
                    None => (rest.to_string(), None),
                };
                if bools.contains(&name.as_str()) {
                    match inline {
                        None => {
                            values.insert(name, "true".to_string());
                        }
                        Some(v) if v == "true" => {
                            values.insert(name, v);
                        }
                        Some(v) if v == "false" => {
                            values.remove(&name);
                        }
                        Some(v) => {
                            return Err(format!("invalid boolean value \"{}\" for -{}", v, name));
                        }
                    }
                    i += 1;
                } else if let Some(v) = inline {
                    values.insert(name, v);
                    i += 1;
                } else if i + 1 < args.len() {
                    values.insert(name, args[i + 1].clone());
                    i += 2;
                } else {
                    return Err(format!("flag needs an argument: -{}", name));
                }
            }
        }
    }
    for name in values.keys() {
        if !known.contains(&name.as_str()) && !bools.contains(&name.as_str()) {
            return Err(format!("flag provided but not defined: -{}", name));
        }
    }
    Ok(Flags {
        values,
        positionals,
    })
}

fn strip_dashes(arg: &str) -> Option<&str> {
    if let Some(rest) = arg.strip_prefix("--") {
        if !rest.is_empty() {
            return Some(rest);
        }
        return None;
    }
    if let Some(rest) = arg.strip_prefix('-') {
        if !rest.is_empty() {
            return Some(rest);
        }
    }
    None
}

pub fn die_err(e: &StoreError) -> ! {
    eprintln!("zhis: {}", e);
    exit(1);
}

pub fn die_usage(msg: &str) -> ! {
    eprintln!("{}", msg);
    exit(2);
}

fn parse_int(name: &str, v: &str) -> i64 {
    match v.parse::<i64>() {
        Ok(n) => n,
        Err(_) => {
            eprintln!("invalid value \"{}\" for flag -{}: parse error", v, name);
            exit(2);
        }
    }
}

pub fn flags_or_die(args: &[String], known: &[&str], bools: &[&str]) -> Flags {
    match parse_flags(args, known, bools) {
        Ok(f) => f,
        Err(e) => die_usage(&e),
    }
}

pub fn int_flag(flags: &Flags, name: &str, default: i64) -> i64 {
    match flags.get(name) {
        Some(v) => parse_int(name, v),
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_parsing() {
        let args: Vec<String> = ["-dir", "/tmp", "-exit=3", "--ms", "250", "-all"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let flags = parse_flags(&args, &["dir", "exit", "ms"], &["all"]).unwrap();
        assert_eq!(flags.get("dir").unwrap(), "/tmp");
        assert_eq!(flags.get("exit").unwrap(), "3");
        assert_eq!(flags.get("ms").unwrap(), "250");
        assert!(flags.has("all"));
        assert!(flags.positionals.is_empty());

        assert!(parse_flags(&["-bogus".to_string()], &["dir"], &[]).is_err());
        assert!(parse_flags(&["-dir".to_string()], &["dir"], &[]).is_err());
    }
}
