//! nucleus's root key kept in a hardware security module — or any
//! PKCS#11 token: a smart card, a cloud HSM's client library, SoftHSM —
//! (PLAN.md §M14.30 K3): the data keys of the keyring (`<dir>.keyring`)
//! wrapped and unwrapped by an AES-256 key that never leaves the token.
//! Taking the token away (or its PIN, or the key) makes the database
//! unreadable.
//!
//! The key is named by an RFC 7512 URI after the scheme:
//!
//! ```text
//! --root-kms 'pkcs11:token=nucleus;object=root?module-path=/usr/lib/softhsm/libsofthsm2.so&pin-source=file:/etc/nucleus/pin'
//! ```
//!
//! The token by `token` (its label), `serial`, `manufacturer`, `model` or
//! `slot-id` — none when the module has a single token; the key by
//! `object` (its label) and/or `id` (`%`-encoded bytes).  The module's
//! library by `module-path` (else `NUCLEUS_PKCS11_MODULE`); the user PIN
//! by `pin-source` (a file, `file:` or a path), `pin-value`, or
//! `NUCLEUS_PKCS11_PIN` — or none, for a token that needs no login.
//!
//! Opening finds the one secret key so named and refuses, by name, one
//! that is not AES-256 or cannot both encrypt and decrypt.  A data key is
//! encrypted by the token with AES-GCM, a fresh 12-byte IV from the
//! token's generator, and its purpose as the additional authenticated
//! data — so the token itself refuses to decrypt it for another.  A blob
//! is `1 ‖ IV ‖ ciphertext ‖ tag`.  The keyring records the root as
//! `pkcs11:token=<label>;object=<label>[;id=<id>]`, read from the token
//! and the key: the same key however the URI names it, wherever the
//! module lives.
//!
//! A session is opened (and logged into) once and kept; one the token
//! dropped — removed and put back, the handle gone stale — is opened
//! again once before an error is reported.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use cryptoki::context::{CInitializeArgs, CInitializeFlags, Pkcs11};
use cryptoki::error::{Error as P11Error, RvError};
use cryptoki::mechanism::Mechanism;
use cryptoki::mechanism::aead::GcmParams;
use cryptoki::object::{Attribute, AttributeType, KeyType, ObjectClass, ObjectHandle};
use cryptoki::session::{Session, UserType};
use cryptoki::slot::Slot;
use cryptoki::types::AuthPin;
use nucleus_server::keyring::{Key, KeyProvider, register_kms};

/// A blob's first byte: AES-GCM, a 12-byte IV, a 16-byte tag.
const BLOB_VERSION: u8 = 1;
const IV_LEN: usize = 12;
const TAG_LEN: usize = 16;
const BLOB_LEN: usize = 1 + IV_LEN + 32 + TAG_LEN;

/// Lets `--root-kms pkcs11:<…>` use this module.
pub fn register() {
    register_kms("pkcs11", open);
}

/// The key the URI's rest (after `pkcs11:`) names, with the module and
/// the PIN of the environment where the URI names none.
pub fn open(uri: &str) -> Result<Arc<dyn KeyProvider>, String> {
    Pkcs11Key::new(uri, |k| std::env::var(k).ok()).map(|k| Arc::new(k) as Arc<dyn KeyProvider>)
}

/// What a `pkcs11:` URI names.
#[derive(Debug, Default, PartialEq)]
pub struct Name {
    /// The token's label.
    pub token: Option<String>,
    /// The token's serial number.
    pub serial: Option<String>,
    /// The token's manufacturer.
    pub manufacturer: Option<String>,
    /// The token's model.
    pub model: Option<String>,
    /// The slot the token is in.
    pub slot: Option<u64>,
    /// The key's label.
    pub object: Option<String>,
    /// The key's CKA_ID.
    pub id: Option<Vec<u8>>,
    /// The module's library.
    pub module: Option<String>,
    /// A file holding the user PIN.
    pub pin_source: Option<String>,
    /// The user PIN.
    pub pin_value: Option<String>,
}

/// Parses the rest of a `pkcs11:` URI (RFC 7512): `;`-separated path
/// attributes, then `?` and `&`-separated query attributes, values
/// `%`-encoded.  An attribute it does not know is refused by name rather
/// than ignored — a key not found for a misspelled `objet` would be the
/// worse failure — but for the query's vendor `x-…` ones.
pub fn parse(rest: &str) -> Result<Name, String> {
    let rest = rest.strip_prefix("pkcs11:").unwrap_or(rest);
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut n = Name::default();
    let text = |v: &str| -> Result<String, String> {
        String::from_utf8(pct_decode(v)?).map_err(|_| format!("{v}: not UTF-8"))
    };
    for attr in path.split(';').filter(|a| !a.is_empty()) {
        let (k, v) = attr
            .split_once('=')
            .ok_or_else(|| format!("pkcs11:{rest}: {attr} has no value"))?;
        let slot = match k {
            "token" => &mut n.token,
            "serial" => &mut n.serial,
            "manufacturer" => &mut n.manufacturer,
            "model" => &mut n.model,
            "object" => &mut n.object,
            "id" => {
                n.id = Some(pct_decode(v)?);
                continue;
            }
            "slot-id" => {
                n.slot = Some(
                    text(v)?
                        .parse()
                        .map_err(|_| format!("slot-id={v}: not a number"))?,
                );
                continue;
            }
            "type" => match v {
                "secret-key" => continue,
                _ => return Err(format!("type={v}: the root is a secret key")),
            },
            _ => return Err(format!("pkcs11:{rest}: no path attribute {k}")),
        };
        *slot = Some(text(v)?);
    }
    for attr in query.split('&').filter(|a| !a.is_empty()) {
        let (k, v) = attr
            .split_once('=')
            .ok_or_else(|| format!("pkcs11:{rest}: {attr} has no value"))?;
        let slot = match k {
            "module-path" => &mut n.module,
            "pin-source" => &mut n.pin_source,
            "pin-value" => &mut n.pin_value,
            _ if k.starts_with("x-") => continue,
            _ => return Err(format!("pkcs11:{rest}: no query attribute {k}")),
        };
        *slot = Some(text(v)?);
    }
    if n.object.is_none() && n.id.is_none() {
        return Err(format!(
            "pkcs11:{rest}: names no key (object=<label> or id=<bytes>)"
        ));
    }
    Ok(n)
}

fn pct_decode(v: &str) -> Result<Vec<u8>, String> {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = v
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("{v}: a % not followed by two hex digits"))?;
            out.push(h);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Ok(out)
}

/// `%`-encodes all but RFC 7512's unreserved path characters.
fn pct_encode(v: &[u8]) -> String {
    let mut s = String::new();
    for &c in v {
        if c.is_ascii_alphanumeric() || b"-._~:[]@!$'()*+,=&".contains(&c) {
            s.push(c as char);
        } else {
            s.push_str(&format!("%{c:02x}"));
        }
    }
    s
}

/// The module's library, loaded and initialized once a process: a
/// second `C_Initialize` is refused, and finalizing would end every
/// other holder's sessions.
fn context(module: &Path) -> Result<Pkcs11, String> {
    static LOADED: OnceLock<Mutex<HashMap<PathBuf, Pkcs11>>> = OnceLock::new();
    let mut loaded = LOADED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(c) = loaded.get(module) {
        return Ok(c.clone());
    }
    let c = Pkcs11::new(module)
        .map_err(|e| format!("the PKCS#11 module {} not loaded: {e}", module.display()))?;
    match c.initialize(CInitializeArgs::new(CInitializeFlags::OS_LOCKING_OK)) {
        Ok(()) | Err(P11Error::Pkcs11(RvError::CryptokiAlreadyInitialized, _)) => {}
        Err(e) => return Err(format!("{}: {}", module.display(), said(&e))),
    }
    loaded.insert(module.to_path_buf(), c.clone());
    Ok(c)
}

/// A PKCS#11 error as `C_<function>: <CKR name>` — the return value
/// named as the token's manual names it, not paraphrased.
fn said(e: &P11Error) -> String {
    match e {
        P11Error::Pkcs11(rv, f) => format!("C_{f:?}: {rv:?}"),
        e => e.to_string(),
    }
}

/// An error that a fresh session might not meet: the token removed and
/// put back, the session or the key's handle gone stale.
fn stale(e: &P11Error) -> bool {
    matches!(
        e,
        P11Error::Pkcs11(
            RvError::SessionHandleInvalid
                | RvError::SessionClosed
                | RvError::DeviceRemoved
                | RvError::DeviceError
                | RvError::TokenNotPresent
                | RvError::UserNotLoggedIn
                | RvError::ObjectHandleInvalid
                | RvError::KeyHandleInvalid,
            _
        )
    )
}

/// An AES-256 key in a PKCS#11 token.
pub struct Pkcs11Key {
    module: PathBuf,
    ctx: Pkcs11,
    name: Name,
    pin: Option<AuthPin>,
    /// The token's label and the key's, as read: the root's id.
    token_label: String,
    key_label: String,
    key_id: Vec<u8>,
    /// A session logged in, and the key's handle in it.
    open: Mutex<Option<(Session, ObjectHandle)>>,
}

impl Pkcs11Key {
    /// The key `uri` names (the part after `pkcs11:`, or all of it), with
    /// `env` read for the module and the PIN the URI names none of.
    pub fn new(uri: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let name = parse(uri)?;
        let module = name
            .module
            .clone()
            .or_else(|| env("NUCLEUS_PKCS11_MODULE"))
            .ok_or(
                "no PKCS#11 module: name its library by module-path= or NUCLEUS_PKCS11_MODULE",
            )?;
        let module = PathBuf::from(module);
        let pin = match (&name.pin_value, &name.pin_source) {
            (Some(v), _) => Some(v.clone()),
            (None, Some(src)) => {
                let path = src.strip_prefix("file:").unwrap_or(src);
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("the PIN not read from {path}: {e}"))?;
                Some(text.trim_end_matches(['\r', '\n']).to_string())
            }
            (None, None) => env("NUCLEUS_PKCS11_PIN"),
        };
        let ctx = context(&module)?;
        let mut key = Pkcs11Key {
            module,
            ctx,
            name,
            pin: pin.map(AuthPin::from),
            token_label: String::new(),
            key_label: String::new(),
            key_id: Vec::new(),
            open: Mutex::new(None),
        };
        let (session, handle) = key.connect()?;
        let attrs = session
            .get_attributes(
                handle,
                &[
                    AttributeType::KeyType,
                    AttributeType::Encrypt,
                    AttributeType::Decrypt,
                    AttributeType::ValueLen,
                    AttributeType::Label,
                    AttributeType::Id,
                ],
            )
            .map_err(|e| format!("{}: {}", key.describe(), said(&e)))?;
        let (mut aes, mut enc, mut dec, mut len) = (false, false, false, None);
        for a in attrs {
            match a {
                Attribute::KeyType(t) => aes = t == KeyType::AES,
                Attribute::Encrypt(b) => enc = b,
                Attribute::Decrypt(b) => dec = b,
                Attribute::ValueLen(l) => len = Some(*l),
                Attribute::Label(l) => key.key_label = String::from_utf8_lossy(&l).into_owned(),
                Attribute::Id(i) => key.key_id = i,
                _ => {}
            }
        }
        if !aes {
            return Err(format!("{}: not an AES key", key.describe()));
        }
        if len.is_some_and(|l| l != 32) {
            return Err(format!(
                "{}: an AES-{} key, not AES-256",
                key.describe(),
                len.unwrap_or(0) * 8
            ));
        }
        if !(enc && dec) {
            return Err(format!(
                "{}: cannot both encrypt and decrypt (CKA_ENCRYPT and CKA_DECRYPT)",
                key.describe()
            ));
        }
        *key.open.lock().unwrap_or_else(|e| e.into_inner()) = Some((session, handle));
        Ok(key)
    }

    /// Finds the token, opens a session on it (logged in, with a PIN),
    /// and finds the key.
    fn connect(&mut self) -> Result<(Session, ObjectHandle), String> {
        let slot = self.slot()?;
        let info = self.ctx.get_token_info(slot).map_err(|e| said(&e))?;
        self.token_label = info.label().trim_end().to_string();
        self.session(slot)
    }

    fn slot(&self) -> Result<Slot, String> {
        let n = &self.name;
        let slots = self
            .ctx
            .get_slots_with_token()
            .map_err(|e| format!("{}: {}", self.module.display(), said(&e)))?;
        let mut found = Vec::new();
        for s in slots {
            if n.slot.is_some_and(|id| id != s.id()) {
                continue;
            }
            // (A slot with a token not initialized — SoftHSM's spare
            // one — holds no key.)
            let Ok(info) = self.ctx.get_token_info(s) else {
                continue;
            };
            if !info.token_initialized() {
                continue;
            }
            let is = |want: &Option<String>, have: &str| {
                want.as_deref().is_none_or(|w| w == have.trim_end())
            };
            if is(&n.token, info.label())
                && is(&n.serial, info.serial_number())
                && is(&n.manufacturer, info.manufacturer_id())
                && is(&n.model, info.model())
            {
                found.push(s);
            }
        }
        match found[..] {
            [s] => Ok(s),
            [] => Err(format!(
                "no token {} in the PKCS#11 module {}",
                self.token_named(),
                self.module.display()
            )),
            _ => Err(format!(
                "{} tokens {} in the PKCS#11 module {}: name one by token=, serial= or slot-id=",
                found.len(),
                self.token_named(),
                self.module.display()
            )),
        }
    }

    fn token_named(&self) -> String {
        let n = &self.name;
        let mut s = Vec::new();
        for (k, v) in [
            ("token", &n.token),
            ("serial", &n.serial),
            ("manufacturer", &n.manufacturer),
            ("model", &n.model),
        ] {
            if let Some(v) = v {
                s.push(format!("{k}={v}"));
            }
        }
        if let Some(id) = n.slot {
            s.push(format!("slot-id={id}"));
        }
        if s.is_empty() {
            "at all".into()
        } else {
            s.join(";")
        }
    }

    fn session(&self, slot: Slot) -> Result<(Session, ObjectHandle), String> {
        let at = || format!("the token {}", self.token_label);
        let session = self
            .ctx
            .open_ro_session(slot)
            .map_err(|e| format!("{}: {}", at(), said(&e)))?;
        if let Some(pin) = &self.pin {
            match session.login(UserType::User, Some(pin)) {
                Ok(()) | Err(P11Error::Pkcs11(RvError::UserAlreadyLoggedIn, _)) => {}
                Err(e) => return Err(format!("{}: {}", at(), said(&e))),
            }
        }
        let mut template = vec![Attribute::Class(ObjectClass::SECRET_KEY)];
        if let Some(l) = &self.name.object {
            template.push(Attribute::Label(l.as_bytes().to_vec()));
        }
        if let Some(i) = &self.name.id {
            template.push(Attribute::Id(i.clone()));
        }
        let keys = session
            .find_objects(&template)
            .map_err(|e| format!("{}: {}", at(), said(&e)))?;
        let login = if self.pin.is_none() {
            " (no PIN given: pin-source=, pin-value= or NUCLEUS_PKCS11_PIN)"
        } else {
            ""
        };
        match keys[..] {
            [k] => Ok((session, k)),
            [] => Err(format!(
                "{}: no secret key {}{login}",
                at(),
                self.key_named()
            )),
            _ => Err(format!(
                "{}: {} secret keys {}: name one by object= and id=",
                at(),
                keys.len(),
                self.key_named()
            )),
        }
    }

    fn key_named(&self) -> String {
        let mut s = Vec::new();
        if let Some(l) = &self.name.object {
            s.push(format!("object={l}"));
        }
        if let Some(i) = &self.name.id {
            s.push(format!("id={}", pct_encode_all(i)));
        }
        s.join(";")
    }

    /// Runs `op` on the kept session; one gone stale is opened again
    /// (the token found anew: it may be in another slot) and `op` tried
    /// once more.
    fn with_session<T>(
        &self,
        op: impl Fn(&Session, ObjectHandle) -> Result<T, P11Error>,
    ) -> Result<T, String> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let mut retried = false;
        loop {
            if open.is_none() {
                let slot = self.slot()?;
                *open = Some(self.session(slot)?);
            }
            let (session, key) = open.as_ref().expect("opened above");
            match op(session, *key) {
                Ok(t) => return Ok(t),
                Err(e) if stale(&e) && !retried => {
                    *open = None;
                    retried = true;
                }
                Err(e) => {
                    if stale(&e) {
                        *open = None;
                    }
                    return Err(format!("{}: {}", self.describe(), said(&e)));
                }
            }
        }
    }
}

/// Every byte `%`-encoded, as RFC 7512 writes an `id`.
fn pct_encode_all(v: &[u8]) -> String {
    v.iter().map(|c| format!("%{c:02x}")).collect()
}

impl KeyProvider for Pkcs11Key {
    fn id(&self) -> String {
        let mut id = format!("pkcs11:token={}", pct_encode(self.token_label.as_bytes()));
        if !self.key_label.is_empty() {
            id.push_str(&format!(
                ";object={}",
                pct_encode(self.key_label.as_bytes())
            ));
        }
        if !self.key_id.is_empty() {
            id.push_str(&format!(";id={}", pct_encode_all(&self.key_id)));
        }
        id
    }

    fn kind(&self) -> String {
        "kms:pkcs11".into()
    }

    fn describe(&self) -> String {
        format!(
            "the PKCS#11 key {} in the token {} ({})",
            self.key_named(),
            self.token_label,
            self.module.display()
        )
    }

    fn wrap(&self, purpose: &str, dek: &Key) -> Result<Vec<u8>, String> {
        self.with_session(|s, key| {
            let iv = s.generate_random_vec(IV_LEN as u32)?;
            let mut param_iv = iv.clone();
            let params = GcmParams::new(&mut param_iv, purpose.as_bytes(), 128.into())?;
            let sealed = s.encrypt(&Mechanism::AesGcm(params), key, dek.bytes())?;
            let mut blob = Vec::with_capacity(BLOB_LEN);
            blob.push(BLOB_VERSION);
            blob.extend_from_slice(&iv);
            blob.extend_from_slice(&sealed);
            Ok(blob)
        })
    }

    fn unwrap(&self, purpose: &str, blob: &[u8]) -> Result<Key, String> {
        if blob.len() != BLOB_LEN || blob[0] != BLOB_VERSION {
            return Err("not a data key wrapped by a PKCS#11 key".into());
        }
        let mut plain = self.with_session(|s, key| {
            let mut iv = blob[1..1 + IV_LEN].to_vec();
            let params = GcmParams::new(&mut iv, purpose.as_bytes(), 128.into())?;
            s.decrypt(&Mechanism::AesGcm(params), key, &blob[1 + IV_LEN..])
        })?;
        let bytes: Result<[u8; 32], _> = plain.as_slice().try_into();
        let n = plain.len();
        plain.fill(0);
        let mut bytes =
            bytes.map_err(|_| format!("the token decrypted {n} bytes, not a key's 32"))?;
        let key = Key::new(bytes);
        bytes.fill(0);
        Ok(key)
    }
}

#[cfg(test)]
mod tests;
