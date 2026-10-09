//! Stand-ins of a safe diagnose bundle (spec 3.2): the same real value gets the same stand-in
//! everywhere in one bundle, and another one in the next bundle.

use std::collections::HashMap;

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// The device's fixed folders. They keep their names; a case variant gets a stand-in.
pub const FIXED_FOLDERS: [&str; 8] = [
    "NOTE",
    "Note",
    "DOCUMENT",
    "Document",
    "EXPORT",
    "INBOX",
    "SCREENSHOT",
    "MyStyle",
];

/// The longest extension that is kept (ASCII letters and digits only).
const MAX_EXTENSION: usize = 5;

/// The stand-ins of one bundle. The HMAC key lives only in memory.
pub struct StandIns {
    key: [u8; 32],
    /// Hex characters of a name stand-in (4, or 8 after a clash).
    width: usize,
    names: HashMap<String, String>,
    /// Stand-in (without extension) to the segment that got it first.
    taken: HashMap<String, String>,
    clashed: bool,
    devices: Counter,
    accounts: Counter,
    ips: Counter,
}

/// `<prefix>-1`, `<prefix>-2`, ... in order of first appearance.
struct Counter {
    prefix: &'static str,
    map: HashMap<String, String>,
}

impl Counter {
    fn new(prefix: &'static str) -> Counter {
        Counter {
            prefix,
            map: HashMap::new(),
        }
    }

    fn get(&mut self, real: &str) -> String {
        let n = self.map.len() + 1;
        self.map
            .entry(real.to_string())
            .or_insert_with(|| format!("{}-{n}", self.prefix))
            .clone()
    }
}

impl StandIns {
    /// Stand-ins with a fresh random key; `width` hex characters per name.
    pub fn new(width: usize) -> StandIns {
        let mut key = [0u8; 32];
        rand::fill(&mut key[..]);
        StandIns::with_key(key, width)
    }

    /// Stand-ins with a given key, for tests.
    pub fn with_key(key: [u8; 32], width: usize) -> StandIns {
        StandIns {
            key,
            width: width.clamp(1, 64),
            names: HashMap::new(),
            taken: HashMap::new(),
            clashed: false,
            devices: Counter::new("dev"),
            accounts: Counter::new("acct"),
            ips: Counter::new("ip"),
        }
    }

    /// The key, so a bundle that must start again with a wider stand-in keeps the same key.
    pub fn key(&self) -> [u8; 32] {
        self.key
    }

    /// True when two different segments got the same stand-in.
    pub fn clashed(&self) -> bool {
        self.clashed
    }

    /// The stand-in of one name segment: `n-` plus hex, and the extension when it is short
    /// ASCII. Fixed folders, digits only (the time folders of the recycle bin) and empty
    /// segments stay.
    pub fn segment(&mut self, s: &str) -> String {
        if s.is_empty() || FIXED_FOLDERS.contains(&s) || s.bytes().all(|b| b.is_ascii_digit()) {
            return s.to_string();
        }
        if let Some(done) = self.names.get(s) {
            return done.clone();
        }
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.key)
            .expect("hmac accepts any key length");
        mac.update(s.as_bytes());
        let hex = const_hex::encode(mac.finalize().into_bytes());
        let stem = format!("n-{}", &hex[..self.width]);
        match self.taken.get(&stem) {
            Some(other) if other != s => self.clashed = true,
            Some(_) => {}
            None => {
                self.taken.insert(stem.clone(), s.to_string());
            }
        }
        let out = match extension(s) {
            Some(ext) => format!("{stem}.{ext}"),
            None => stem,
        };
        self.names.insert(s.to_string(), out.clone());
        out
    }

    /// Every segment of a `/` path gets its stand-in; leading, trailing and double slashes stay.
    pub fn path(&mut self, p: &str) -> String {
        p.split('/')
            .map(|seg| self.segment(seg))
            .collect::<Vec<_>>()
            .join("/")
    }

    pub fn device(&mut self, serial: &str) -> String {
        self.devices.get(serial)
    }

    pub fn account(&mut self, slug: &str) -> String {
        self.accounts.get(slug)
    }

    pub fn ip(&mut self, ip: &str) -> String {
        self.ips.get(ip)
    }

    /// Every real segment that got a stand-in so far.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.names.keys().map(String::as_str)
    }
}

/// The extension after the last dot, when it is 1 to 5 ASCII letters or digits and something
/// comes before the dot.
fn extension(s: &str) -> Option<&str> {
    let (stem, ext) = s.rsplit_once('.')?;
    let ok = !stem.is_empty()
        && !ext.is_empty()
        && ext.len() <= MAX_EXTENSION
        && ext.bytes().all(|b| b.is_ascii_alphanumeric());
    ok.then_some(ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_stand_in(s: &str, width: usize) -> bool {
        let Some(rest) = s.strip_prefix("n-") else {
            return false;
        };
        let hex = rest.split('.').next().unwrap();
        hex.len() == width && hex.bytes().all(|b| b.is_ascii_hexdigit())
    }

    #[test]
    fn the_same_segment_gets_the_same_stand_in() {
        let mut s = StandIns::new(4);
        let a = s.segment("Planview");
        assert!(is_stand_in(&a, 4), "{a}");
        assert_eq!(s.segment("Planview"), a);
        assert_ne!(s.segment("Planview2"), a);
    }

    #[test]
    fn two_bundles_give_different_stand_ins() {
        let mut a = StandIns::new(8);
        let mut b = StandIns::new(8);
        let names = ["one", "two", "three"];
        let same = names.iter().all(|n| a.segment(n) == b.segment(n));
        assert!(!same);
    }

    #[test]
    fn the_extension_is_kept() {
        let mut s = StandIns::new(4);
        let out = s.segment("Plainview Overview.note");
        assert!(out.ends_with(".note"), "{out}");
        assert!(is_stand_in(&out, 4), "{out}");
        assert!(s.segment("a.PDF").ends_with(".PDF"));
    }

    #[test]
    fn a_long_or_odd_extension_is_not_kept() {
        let mut s = StandIns::new(4);
        for name in [
            "Meeting 3.5 Smith",
            "x.tar-gz",
            "a.verylongext",
            ".hidden",
            "end.",
        ] {
            let out = s.segment(name);
            assert!(!out.contains('.'), "{name} gave {out}");
            assert!(is_stand_in(&out, 4), "{name} gave {out}");
        }
    }

    #[test]
    fn fixed_folders_keep_their_names() {
        let mut s = StandIns::new(4);
        for f in FIXED_FOLDERS {
            assert_eq!(s.segment(f), f);
        }
        assert_eq!(s.path("/NOTE/Note/"), "/NOTE/Note/");
        assert_eq!(s.segment("1791466487188"), "1791466487188");
    }

    #[test]
    fn a_case_variant_of_a_fixed_folder_is_replaced() {
        let mut s = StandIns::new(4);
        for name in ["note", "NOTE ", "document"] {
            assert!(is_stand_in(&s.segment(name), 4), "{name}");
        }
    }

    #[test]
    fn a_path_keeps_its_slashes() {
        let mut s = StandIns::new(4);
        let out = s.path("/NOTE/Note/Planview/Plainview Overview.note");
        let parts: Vec<_> = out.split('/').collect();
        assert_eq!(parts[..3], ["", "NOTE", "Note"]);
        assert!(is_stand_in(parts[3], 4));
        assert!(parts[4].ends_with(".note"));
    }

    #[test]
    fn a_clash_is_reported() {
        let names: Vec<String> = (0..17).map(|i| format!("name {i}")).collect();
        let mut narrow = StandIns::with_key([7; 32], 1);
        for n in &names {
            narrow.segment(n);
        }
        assert!(narrow.clashed());
        let mut wide = StandIns::with_key([7; 32], 8);
        for n in &names {
            wide.segment(n);
        }
        assert!(!wide.clashed());
    }

    #[test]
    fn devices_accounts_and_ips_count_up() {
        let mut s = StandIns::new(4);
        assert_eq!(s.device("SN1"), "dev-1");
        assert_eq!(s.device("SN2"), "dev-2");
        assert_eq!(s.device("SN1"), "dev-1");
        assert_eq!(s.account("jordi"), "acct-1");
        assert_eq!(s.ip("10.0.0.1"), "ip-1");
        assert_eq!(s.ip("::1"), "ip-2");
    }

    #[test]
    fn names_lists_the_replaced_segments() {
        let mut s = StandIns::new(4);
        s.path("NOTE/Secret/a.note");
        let mut names: Vec<_> = s.names().collect();
        names.sort();
        assert_eq!(names, ["Secret", "a.note"]);
    }
}
