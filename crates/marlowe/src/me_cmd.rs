//! `marlowe me [--name N] [--address sir|name|none]`: what Marlowe calls you.

use std::path::Path;
use std::time::Duration;

use marlowe_proto::{clean_user_field, AddressForm, Event, Request};

use crate::client::Client;
use crate::daemon_ctl;

const OP_TIMEOUT: Duration = Duration::from_secs(30);

pub const ME_USAGE: &str = "usage: marlowe me [--name N] [--address sir|name|none]";

/// The user as the daemon reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    pub name: Option<String>,
    pub address: Option<String>,
    pub set: bool,
}

/// Parsed flags. `name: Some("")` clears the name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub name: Option<String>,
    pub address: Option<AddressForm>,
}

pub fn parse(args: &[String]) -> Result<Flags, String> {
    let mut f = Flags::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("{a} needs a value\n{ME_USAGE}"));
        match a.as_str() {
            "--name" => f.name = Some(value()?),
            "--address" => f.address = Some(value()?.parse().map_err(|e| format!("--address: {e}"))?),
            other => return Err(format!("unknown argument {other:?}\n{ME_USAGE}")),
        }
    }
    Ok(f)
}

/// The `SetUser` to send. A name-based address follows a rename; a custom address (from env)
/// is kept unless `--address` replaces it. Someone never set up defaults to "sir", as in the TUI.
pub fn plan(cur: &Current, f: &Flags) -> Result<(Option<String>, Option<String>), String> {
    let name = match &f.name {
        Some(n) => clean_user_field("name", Some(n))?,
        None => cur.name.clone(),
    };
    let form = match f.address {
        Some(a) => Some(a),
        None if !cur.set => Some(AddressForm::Sir),
        None => AddressForm::classify(cur.address.as_deref(), cur.name.as_deref()),
    };
    if form == Some(AddressForm::Name) && name.is_none() {
        return Err("--address name needs a name: marlowe me --name N --address name".into());
    }
    let address = match form {
        Some(a) => a.resolve(name.as_deref()),
        None => cur.address.clone(),
    };
    Ok((name, address))
}

pub fn describe(cur: &Current) -> String {
    let name = cur.name.as_deref().unwrap_or("not set");
    let address = match AddressForm::classify(cur.address.as_deref(), cur.name.as_deref()) {
        Some(AddressForm::Sir) => "sir".to_string(),
        Some(AddressForm::Name) => format!("{name} (your name)"),
        Some(AddressForm::Nothing) => "none".to_string(),
        None => cur.address.clone().unwrap_or_default(),
    };
    let mut s = format!("name     {name}\naddress  {address}");
    if !cur.set {
        s.push_str(&format!("\nnot set up yet · {}", ME_USAGE.trim_start_matches("usage: ")));
    }
    s
}

fn user_of(e: &Event) -> Option<Current> {
    match e {
        Event::User { name, address, set } => Some(Current { name: name.clone(), address: address.clone(), set: *set }),
        _ => None,
    }
}

pub fn run(home: &Path, args: &[String]) -> Result<i32, String> {
    let flags = parse(args)?;
    let info = daemon_ctl::ensure(home)?;
    let mut c = Client::connect(&info, "marlowe-me").map_err(|e| e.to_string())?;
    // The Welcome flow sends the current user.
    let cur = c.wait_for(OP_TIMEOUT, user_of).map_err(|e| e.to_string())??;
    if flags == Flags::default() {
        println!("{}", describe(&cur));
        return Ok(0);
    }
    let (name, address) = plan(&cur, &flags)?;
    c.send(&Request::SetUser { name: name.clone(), address: address.clone() }).map_err(|e| e.to_string())?;
    let now = c
        .wait_for(OP_TIMEOUT, |e| user_of(e).filter(|u| u.set && u.name == name && u.address == address))
        .map_err(|e| e.to_string())??;
    println!("{}", describe(&now));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cur(name: Option<&str>, address: Option<&str>, set: bool) -> Current {
        Current { name: name.map(Into::into), address: address.map(Into::into), set }
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_parse_and_refuse_nonsense() {
        assert_eq!(parse(&[]).unwrap(), Flags::default());
        let f = parse(&args(&["--name", "Matthew", "--address", "name"])).unwrap();
        assert_eq!(f, Flags { name: Some("Matthew".into()), address: Some(AddressForm::Name) });
        assert!(parse(&args(&["--address", "madam"])).is_err());
        assert!(parse(&args(&["--name"])).is_err());
        assert!(parse(&args(&["Matthew"])).is_err());
    }

    #[test]
    fn plans_merge_with_what_is_there() {
        let never = cur(None, None, false);
        let name_only = Flags { name: Some("Matthew".into()), address: None };
        // First time: the address defaults to sir, as in the TUI.
        assert_eq!(plan(&never, &name_only).unwrap(), (Some("Matthew".into()), Some("sir".into())));
        // A name-based address follows a rename.
        let by_name = cur(Some("Matt"), Some("Matt"), true);
        assert_eq!(plan(&by_name, &name_only).unwrap(), (Some("Matthew".into()), Some("Matthew".into())));
        // A chosen "none" stays none.
        let plain = cur(Some("Matt"), None, true);
        assert_eq!(plan(&plain, &name_only).unwrap(), (Some("Matthew".into()), None));
        // A custom form from env is kept.
        let custom = cur(Some("Matt"), Some("Doctor"), true);
        assert_eq!(plan(&custom, &name_only).unwrap().1.as_deref(), Some("Doctor"));
        // --address alone; --name "" clears the name; name without a name is refused.
        let sir = Flags { name: None, address: Some(AddressForm::Nothing) };
        assert_eq!(plan(&by_name, &sir).unwrap(), (Some("Matt".into()), None));
        let clear = Flags { name: Some(String::new()), address: Some(AddressForm::Sir) };
        assert_eq!(plan(&by_name, &clear).unwrap(), (None, Some("sir".into())));
        let bad = Flags { name: Some(String::new()), address: Some(AddressForm::Name) };
        assert!(plan(&by_name, &bad).is_err());
    }

    #[test]
    fn describe_reads_flat() {
        assert_eq!(describe(&cur(Some("Matthew"), Some("sir"), true)), "name     Matthew\naddress  sir");
        assert_eq!(describe(&cur(Some("Matthew"), Some("Matthew"), true)), "name     Matthew\naddress  Matthew (your name)");
        let unset = describe(&cur(None, None, false));
        assert!(unset.starts_with("name     not set\naddress  none\nnot set up yet · marlowe me"), "{unset}");
    }
}
