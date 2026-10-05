//! Three small host modules for what Cove cannot compute itself: the time,
//! randomness, and a check against a secret it should never hold.
//!
//! | operation | answers |
//! | --- | --- |
//! | `time.nowMillis()` | `Int`: milliseconds since the Unix epoch, from the wall clock |
//! | `time.nowMicros()` | `Int`: microseconds since the Unix epoch, **strictly increasing** across every call in the process (one more than the last when the clock has not moved on or went back), so it orders, and names, things made in the same millisecond |
//! | `random.hex(bytes)` | `String`: `bytes` (1–64) random bytes from the operating system, as lowercase hex |
//! | `auth.check(secret, authorization)` | `Bool`: whether an `Authorization` header value presents the app's secret `secret` |
//! | `auth.identity(headers)` | `Result<auth.Identity, Error>`: who is asking — a verified Cloudflare Access user, or the `[access] token` ([`crate::access`]) |
//! | `auth.usesAccess()` | `Bool`: whether Access is on for this app ([`crate::access`]) |
//!
//! `auth.check` takes the header as the client sent it and accepts
//! `Bearer <secret>` and `Basic <base64(user:secret)>` (any user name, so a
//! browser's login prompt works). The comparison takes time that depends on
//! the lengths only, and the secret never enters the run: an app can check a
//! credential, not read one, so neither its code, its logs nor its errors
//! can leak it. Secrets are `[secrets]` in `app.toml`, from an environment
//! variable, a file, or (for tests) a literal. An unknown secret name is
//! `false`.
//!
//! All three answer at once, but for an `auth.identity` that has to fetch
//! Access's keys first, which parks.

use std::io::Read;

use std::sync::Arc;

use cove_runtime::{
    Effect, FieldSchema, HostAnswer, HostApi, HostType, ModuleSchema, OperationSchema, Reentry,
    RuntimeError, Transfer, TypeSchema, Value,
};

use crate::access::{Gate, Presented, Step, Verifier};
use crate::hosts::{AppContext, HostModule, PendingWork};

const fn op(
    name: &'static str,
    params: &'static [HostType],
    result: HostType,
    capability: &'static str,
) -> OperationSchema {
    OperationSchema {
        name,
        params,
        variadic: false,
        result,
        capability,
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }
}

/// The `time` module.
pub const TIME: ModuleSchema = ModuleSchema {
    name: "time",
    capability: "time",
    operations: &[
        op("nowMillis", &[], HostType::Int, "time"),
        op("nowMicros", &[], HostType::Int, "time"),
    ],
    types: &[],
    resources: &[],
};

/// The `random` module.
pub const RANDOM: ModuleSchema = ModuleSchema {
    name: "random",
    capability: "random",
    operations: &[op("hex", &[HostType::Int], HostType::String, "random")],
    types: &[],
    resources: &[],
};

/// The `auth` module.
pub const AUTH: ModuleSchema = ModuleSchema {
    name: "auth",
    capability: "auth",
    operations: &[
        op(
            "check",
            &[HostType::String, HostType::String],
            HostType::Bool,
            "auth",
        ),
        op(
            "identity",
            &[HostType::Map(&HostType::String, &HostType::String)],
            HostType::Result(&HostType::Named("auth.Identity"), &HostType::Error),
            "auth",
        ),
        op("usesAccess", &[], HostType::Bool, "auth"),
    ],
    types: &[TypeSchema {
        name: "Identity",
        cases: &[],
        fields: &[
            FieldSchema {
                name: "email",
                ty: HostType::String,
            },
            FieldSchema {
                name: "via",
                ty: HostType::String,
            },
        ],
    }],
    resources: &[],
};

pub(crate) struct TimeModule;
pub(crate) struct RandomModule;
pub(crate) struct AuthModule;

impl HostModule for TimeModule {
    fn schema(&self) -> ModuleSchema {
        TIME
    }
    fn instantiate(&self, _app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        Ok(Box::new(TimeHost))
    }
}

impl HostModule for RandomModule {
    fn schema(&self) -> ModuleSchema {
        RANDOM
    }
    fn instantiate(&self, _app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        Ok(Box::new(RandomHost))
    }
}

impl HostModule for AuthModule {
    fn schema(&self) -> ModuleSchema {
        AUTH
    }
    fn instantiate(&self, app: &AppContext) -> Result<Box<dyn HostApi>, String> {
        let say = {
            let name = app.app.clone();
            let quiet = app.quiet;
            let logs = Arc::clone(&app.logs);
            move |line: &str| {
                logs.push("warn", line);
                if !quiet {
                    println!("[{name}] warn: {line}");
                }
            }
        };
        if let Some(off) = &app.access.off {
            if app.granted.contains("auth") {
                say(&format!("{off}: Cloudflare Access is off for this app"));
            }
        }
        let verifier = match &app.access.jwt {
            Some(policy) if app.granted.contains("auth") => {
                Some(Arc::new(Verifier::new(policy.clone(), Box::new(say))?))
            }
            _ => None,
        };
        Ok(Box::new(AuthHost {
            gate: Arc::new(Gate {
                access: app.access.clone(),
                secrets: app.secrets.clone(),
                verifier,
            }),
            io: app.io.clone(),
        }))
    }
}

struct TimeHost;

impl HostApi for TimeHost {
    fn module_schema(&self) -> ModuleSchema {
        TIME
    }
    fn call(&self, op: &str, _args: Vec<Value>) -> Result<Value, RuntimeError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| RuntimeError::new(format!("time: the clock is before 1970: {e}")))?;
        if op == "nowMicros" {
            return Ok(Value::int(increasing_micros(now.as_micros() as i64)));
        }
        Ok(Value::int(now.as_millis() as i64))
    }
}

/// `now`, or one more than the last answer if that is not later.
fn increasing_micros(now: i64) -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST: AtomicI64 = AtomicI64::new(0);
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(last + 1);
        match LAST.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

struct RandomHost;

/// `bytes` random bytes as lowercase hex.
pub fn random_hex(bytes: usize) -> std::io::Result<String> {
    let mut buffer = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buffer)?;
    Ok(buffer.iter().map(|b| format!("{b:02x}")).collect())
}

impl HostApi for RandomHost {
    fn module_schema(&self) -> ModuleSchema {
        RANDOM
    }
    fn call(&self, _op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let bytes = args.first().and_then(Value::as_int).unwrap_or_default();
        if !(1..=64).contains(&bytes) {
            return Err(RuntimeError::new(format!(
                "random.hex({bytes}): the count must be 1 to 64"
            )));
        }
        random_hex(bytes as usize)
            .map(Value::string)
            .map_err(|e| RuntimeError::new(format!("random: {e}")))
    }
}

struct AuthHost {
    gate: Arc<Gate>,
    io: tokio::runtime::Handle,
}

/// `auth.identity`'s answer, as the `Result` value.
fn identity_answer(identity: Result<crate::access::Identity, String>) -> Transfer {
    match identity {
        Ok(identity) => Transfer::ok(identity.transfer()),
        Err(why) => Transfer::err(Transfer::error(why)),
    }
}

impl AuthHost {
    /// The verification left after [`Gate::begin`], as a future.
    fn verify(
        &self,
        verifier: Arc<Verifier>,
        assertion: String,
        presented: Presented,
    ) -> impl std::future::Future<Output = Result<Transfer, RuntimeError>> + Send + 'static {
        let gate = Arc::clone(&self.gate);
        async move {
            let verified = verifier.verify(&assertion).await;
            Ok(identity_answer(gate.finish(&presented, verified)))
        }
    }
}

impl HostApi for AuthHost {
    fn module_schema(&self) -> ModuleSchema {
        AUTH
    }

    /// The blocking answer; `auth.identity` waits on the worker for a key
    /// fetch here, where the run cannot park.
    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "identity" => {
                let presented = Presented::from_headers(args.first().unwrap_or(&Value::unit()));
                match self.gate.begin(&presented) {
                    Step::Done(identity) => Ok(identity_answer(identity).into_value()),
                    Step::Verify(verifier, assertion) => self
                        .io
                        .block_on(self.verify(verifier, assertion, presented))
                        .map(Transfer::into_value),
                }
            }
            "usesAccess" => Ok(Value::bool(self.gate.verifier.is_some())),
            _ => {
                let text = |at: usize| args.get(at).and_then(Value::as_str).unwrap_or_default();
                let ok = match self.gate.secrets.0.get(text(0)) {
                    Some(secret) => presents(text(1), secret),
                    None => false,
                };
                Ok(Value::bool(ok))
            }
        }
    }

    fn call_parkable(&self, op: &str, args: Vec<Value>, _back: &mut dyn Reentry) -> HostAnswer {
        if op != "identity" {
            return HostAnswer::Ready(self.call(op, args));
        }
        let presented = Presented::from_headers(args.first().unwrap_or(&Value::unit()));
        match self.gate.begin(&presented) {
            Step::Done(identity) => HostAnswer::Ready(Ok(identity_answer(identity).into_value())),
            Step::Verify(verifier, assertion) => {
                PendingWork::new("auth.identity", self.verify(verifier, assertion, presented))
                    .answer()
            }
        }
    }
}

/// Whether `authorization` presents `secret`, as `Bearer <secret>` or
/// `Basic <base64(user:secret)>`.
pub fn presents(authorization: &str, secret: &str) -> bool {
    let authorization = authorization.trim();
    let (scheme, rest) = authorization.split_once(' ').unwrap_or((authorization, ""));
    let rest = rest.trim();
    if scheme.eq_ignore_ascii_case("bearer") {
        return same(rest.as_bytes(), secret.as_bytes());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let Some(decoded) = base64_decode(rest) else {
            return false;
        };
        let password = match decoded.iter().position(|&b| b == b':') {
            Some(colon) => &decoded[colon + 1..],
            None => return false,
        };
        return same(password, secret.as_bytes());
    }
    false
}

/// Equality in time that depends on the lengths only.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// Standard base64, with or without padding; `None` for anything else.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let text = text.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    for chunk in text.chunks(4) {
        if chunk.len() == 1 {
            return None;
        }
        let mut word = 0u32;
        for (at, c) in chunk.iter().enumerate() {
            word |= value(*c)? << (18 - 6 * at);
        }
        out.push((word >> 16) as u8);
        if chunk.len() > 2 {
            out.push((word >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(word as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_and_basic_present_the_secret() {
        assert!(presents("Bearer s3cret", "s3cret"));
        assert!(presents("bearer  s3cret ", "s3cret"));
        assert!(!presents("Bearer s3cre", "s3cret"));
        assert!(!presents("Bearer s3cret2", "s3cret"));
        // base64("admin:s3cret")
        assert!(presents("Basic YWRtaW46czNjcmV0", "s3cret"));
        // base64("anyone:s3cret")
        assert!(presents("Basic YW55b25lOnMzY3JldA==", "s3cret"));
        // base64("admin:wrong")
        assert!(!presents("Basic YWRtaW46d3Jvbmc=", "s3cret"));
        assert!(!presents("Basic !!!", "s3cret"));
        assert!(!presents("", "s3cret"));
        assert!(!presents("s3cret", "s3cret"));
    }

    #[test]
    fn micros_only_go_up() {
        let first = increasing_micros(1_000);
        let second = increasing_micros(1_000);
        let third = increasing_micros(5);
        assert!(second > first && third > second);
    }

    #[test]
    fn decodes_base64() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64_decode("aGk=").unwrap(), b"hi");
        assert!(base64_decode("a").is_none());
    }

    #[test]
    fn random_hex_is_hex_of_the_length_asked() {
        let one = random_hex(8).unwrap();
        assert_eq!(one.len(), 16);
        assert!(one.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(one, random_hex(8).unwrap());
    }
}
