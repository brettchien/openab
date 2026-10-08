//! Redaction for text that leaves core: session-bearing identifiers in logs
//! ([`redact_session_ids`]), and credentials in agent tool titles ([`redact_tool_title`]).
//!
//! An ACP session is addressed two ways and both carry the same uuid: the session id is
//! `sess_<uuid>` and the channel id is `acp_<uuid>`. Either one is enough to resume — `sess_` is
//! taken directly by resume, and `acp_` differs from it only by prefix — so both are credentials,
//! and a redaction that covers one of them covers nothing.
//!
//! Ids also travel embedded: a pool key is `<platform>:<channel_id>`, so scanning for a field
//! named `channel` misses it entirely. Redaction here matches on the VALUE's shape, which is why
//! it can be applied to a composite without the caller taking it apart.
//!
//! Applying this to a non-ACP identifier is a no-op, deliberately. A Discord or Slack channel id
//! is public and operators grep for it, and because the function leaves it untouched, a caller
//! that cannot tell whether a given id will ever be ACP does not have to find out — applying it
//! is free and removes the question. That is cheaper than an investigation and safer than a guess.

use regex::{Captures, Regex};
use std::borrow::Cow;
use std::sync::LazyLock;

/// Hash any `acp_`/`sess_` prefixed segment of `s`, leaving everything else exactly as it was.
///
/// Segments are split on `:` so a `<platform>:<channel_id>` pool key redacts its id half and keeps
/// the platform readable.
///
/// The same uuid tags identically whichever prefix carried it, so one session reads as one tag
/// across every log line that mentions it — that correlation is the only reason to keep an
/// identifier in a log at all.
pub fn redact_session_ids(s: &str) -> String {
    s.split(':')
        .map(|seg| match seg.strip_prefix("acp_").or_else(|| seg.strip_prefix("sess_")) {
            Some(uuid) if !uuid.is_empty() => hash_tag(uuid),
            _ => seg.to_string(),
        })
        .collect::<Vec<_>>()
        .join(":")
}

fn hash_tag(uuid: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(uuid.as_bytes());
    let short: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("#{short}")
}

/// What a redacted secret is replaced with. A redacted title is itself a fixed point of
/// [`redact_tool_title`] (re-redacting changes nothing).
const REDACTED: &str = "***";

/// A secret value after a context prefix (`pre`): bash ANSI-C quoted (`$'…'`, a literal), a `${…}`
/// parameter expansion, double-quoted, single-quoted, or a bare shell word. The bare form stops at
/// quotes, backslashes, and shell/JSON punctuation so the rest of the command stays readable.
/// Alternation is leftmost-first, so `$'…'` and `${…}` win over a bare `$`.
macro_rules! secret_value {
    () => {
        r#"(?:\$'(?P<ac>[^']*)'|\$\{(?P<pe>[^}]*)\}|"(?P<dq>[^"]*)"|'(?P<sq>[^']*)'|(?P<uq>[^\s"'`\\;&|<>(),}]+))"#
    };
}

/// Self-identifying token formats, replaced wholesale. Only the vendor prefix (and, for Discord
/// bot tokens, the first segment — the base64 bot user id, which is public) survives.
static TOKEN_RULES: LazyLock<Vec<(Regex, &str)>> = LazyLock::new(token_rules);

fn token_rules() -> Vec<(Regex, &'static str)> {
    [
        // PEM private key block (e.g. a heredoc); an unterminated block is redacted to the end.
        (
            r"-----BEGIN (?P<k>[A-Z ]*)PRIVATE KEY-----(?s:.*?)(?:-----END [A-Z ]*PRIVATE KEY-----|\z)",
            "-----BEGIN ${k}PRIVATE KEY-----***-----END ${k}PRIVATE KEY-----",
        ),
        // GitHub: classic PAT / OAuth / user-to-server / server-to-server / refresh, fine-grained PAT.
        (r"\b(?P<p>gh[pousr]_)[A-Za-z0-9]{20,}", "${p}***"),
        (r"\bgithub_pat_[A-Za-z0-9_]{20,}", "github_pat_***"),
        // Slack bot / user / app / legacy tokens, app-level tokens.
        (r"\b(?P<p>xox[abposr]-)[A-Za-z0-9-]{10,}", "${p}***"),
        (r"\bxapp-[A-Za-z0-9-]{10,}", "xapp-***"),
        // Anthropic (`sk-ant-…`) / OpenAI (`sk-…`, `sk-proj-…`) API keys.
        (r"\b(?P<p>sk-(?:ant-|proj-)?)[A-Za-z0-9_-]{20,}", "${p}***"),
        // AWS access key ids (long-term / STS).
        (r"\b(?P<p>AKIA|ASIA)[0-9A-Z]{16}\b", "${p}***"),
        // Google API keys.
        (r"\bAIza[0-9A-Za-z_-]{35}", "AIza***"),
        // JWTs (header and payload are base64url JSON, so both start with `eyJ`).
        (r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}", "eyJ***"),
        // Discord bot tokens: base64(user id).timestamp.hmac. A snowflake is ASCII digits, so the
        // first segment starts with M/N/O; the length bounds keep this off ordinary dotted names.
        (
            r"\b(?P<id>[MNO][A-Za-z0-9_-]{23,27})\.[A-Za-z0-9_-]{6,7}\.[A-Za-z0-9_-]{27,}",
            "${id}.***",
        ),
        // Telegram bot tokens: `<bot id>:<secret>`, bare or in an API path (`/bot<id>:<secret>/`).
        // `bot` is inside the match because there is no word boundary between it and the id.
        (r"\b(?P<p>(?:bot)?\d{6,12}:)[A-Za-z0-9_-]{30,}", "${p}***"),
    ]
    .into_iter()
    .map(|(re, rep)| (Regex::new(re).expect("valid token redaction regex"), rep))
    .collect()
}

/// Secrets recognised by their context rather than their shape: the `pre` group is kept, the
/// value (see [`secret_value!`]) is redacted. The flag says a value that looks like the next CLI
/// flag (`-…`) is not a value and is kept.
static VALUE_RULES: LazyLock<Vec<(Regex, bool)>> = LazyLock::new(value_rules);

fn value_rules() -> Vec<(Regex, bool)> {
    [
        // URL userinfo: `scheme://user:pass@host` keeps the user.
        (r"(?i)(?P<pre>\b[a-z][a-z0-9+.-]*://[^/\s:@]+:)(?P<uq>[^/\s@]+)@", false),
        // HTTP Authorization header, with or without a known scheme.
        (
            concat!(
                r"(?i)(?P<pre>\bauthorization\s*:\s*(?:(?:bearer|bot|basic|token|digest|negotiate)\s+)?)",
                secret_value!()
            ),
            false,
        ),
        // Secret-named header / JSON / YAML key: `X-Api-Key: v`, `"password":"v"`, `token: v`.
        // Requires a quote before or whitespace after the colon, which keeps it off URLs.
        (
            concat!(
                r#"(?i)(?P<pre>\b[\w-]*(?:token|secret|passw(?:or)?d|api[-_]?key|access[-_]?key|private[-_]?key|credentials?)(?:["']\s*:\s*|\s*:\s+))"#,
                secret_value!()
            ),
            false,
        ),
        // Secret-named long CLI flag: `--token v`, `--password=v`, `--api-key v`, `--client-secret v`.
        (
            concat!(
                r"(?i)(?P<pre>--[\w-]*?(?:token|secret|password|passwd|api-?key|access-key|private-key|credentials?)(?:=|\s+))",
                secret_value!()
            ),
            true,
        ),
        // Secret-named env / query assignment: `GH_TOKEN=v`, `export DB_PASSWORD='v'`, `?access_token=v`.
        (
            concat!(
                r"(?i)(?P<pre>\b[A-Za-z0-9_]*(?:token|secret|password|passwd|api_?key|access_key|private_key|credential)[A-Za-z0-9_]*=)",
                secret_value!()
            ),
            false,
        ),
    ]
    .into_iter()
    .map(|(re, flag)| (Regex::new(re).expect("valid value redaction regex"), flag))
    .collect()
}

/// A whole value that is just a shell variable reference: `$VAR` or `${VAR}`. `${VAR:-literal}` and
/// friends are not — the default is a literal.
static VAR_REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\$\{?[A-Za-z_][A-Za-z0-9_]*\}?$").expect("valid var regex"));

/// The inside of a `${…}` that is just a variable name.
static VAR_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("valid name regex"));

/// Mask credentials in an agent tool title. With claude-agent-acp the title is often the literal
/// shell command (`curl -H "Authorization: Bot …"`, `export GH_TOKEN=ghp_…`), and it is shown on
/// chat platforms (Discord / Slack tool lines, the assistant status line) and logged, so core
/// masks it once where it parses the agent's event. ACP tool progress does not carry the title at
/// all (see `ToolCallProgress`), so it does not depend on this mask. Best-effort and
/// pattern-based: it errs on over-redaction of secret-named values, never panics (all edits are at
/// regex match boundaries, so non-ASCII text is safe), and is idempotent.
///
/// A shell reference (`"Bot $DISCORD_BOT_TOKEN"`, `${GH_TOKEN}`, `TOKEN=$(cat f)`) is not a secret
/// and stays readable. Single-quoted and `$'…'` values never expand, so they are always redacted.
pub fn redact_tool_title(title: &str) -> String {
    let mut out = title.to_string();
    for (re, rep) in TOKEN_RULES.iter() {
        if let Cow::Owned(s) = re.replace_all(&out, *rep) {
            out = s;
        }
    }
    for (re, flag_aware) in VALUE_RULES.iter() {
        let replaced = re.replace_all(&out, |c: &Captures| {
            let whole = c.get(0).expect("group 0 always matches");
            let is_reference = |v: &str| {
                // A bare `$` cut short by the stop set is a `$(…)` substitution when `(` follows.
                VAR_REFERENCE.is_match(v)
                    || v.starts_with("$(")
                    || (v == "$" && out[whole.end()..].starts_with('('))
            };
            let (value, is_reference) = if let Some(v) = c.name("pe") {
                // `${…}`: a reference when it holds just a name, else (`${X:-literal}`) a literal.
                (v, VAR_NAME.is_match(v.as_str()))
            } else if let Some(v) = c.name("dq").or(c.name("uq")) {
                (v, is_reference(v.as_str()))
            } else if let Some(v) = c.name("ac").or(c.name("sq")) {
                (v, false)
            } else {
                return whole.as_str().to_string();
            };
            let v = value.as_str();
            let is_next_flag = *flag_aware && c.name("uq").is_some() && v.starts_with('-');
            if v.is_empty() || is_reference || is_next_flag {
                return whole.as_str().to_string();
            }
            // Keep everything in the match around the value: the prefix, the quotes, and the
            // URL rule's trailing `@`.
            let (start, end) = (value.start() - whole.start(), value.end() - whole.start());
            let w = whole.as_str();
            format!("{}{REDACTED}{}", &w[..start], &w[end..])
        });
        if let Cow::Owned(s) = replaced {
            out = s;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::redact_session_ids;

    /// A table, not a single vector — the branch structure is what needs pinning.
    ///
    /// One example only proves the hash. The predicate is the part most likely to move: this
    /// function exists because a redaction covering `acp_` but not `sess_` was shipped, and that
    /// edit changes which inputs hash without changing the output for any `acp_` input. A single
    /// `acp_...` vector cannot see it.
    #[test]
    fn both_encodings_hash_alike_and_everything_else_passes_through() {
        let u = "00000000-0000-0000-0000-000000000000";
        let tag = redact_session_ids(&format!("acp_{u}"));
        assert!(tag.starts_with('#') && tag.len() == 9, "expected #<8hex>, got {tag}");
        assert_eq!(
            redact_session_ids(&format!("sess_{u}")),
            tag,
            "both encodings carry the SAME uuid, so they must produce the same tag or one session \
             reads as two"
        );
        assert_eq!(
            redact_session_ids(&format!("acp:acp_{u}")),
            format!("acp:{tag}"),
            "a <platform>:<id> pool key must redact the id half and keep the platform greppable"
        );
        assert_eq!(redact_session_ids("1234567890"), "1234567890", "public ids stay greppable");
        assert_eq!(redact_session_ids("-"), "-", "the no-session sentinel is not a session");
        assert_eq!(redact_session_ids(""), "", "empty in, empty out");
        assert_eq!(redact_session_ids("acp_"), "acp_", "a bare prefix carries no uuid to hide");
        assert_eq!(
            redact_session_ids("discord:1234567890"),
            "discord:1234567890",
            "a non-ACP composite is untouched, which is why applying this blindly is safe"
        );
    }
}

#[cfg(test)]
mod tool_title_tests {
    use super::redact_tool_title;

    /// `(input, expected)` pairs for [`redact_tool_title`], one or more per pattern.
    const REDACTION_CASES: &[(&str, &str)] = &[
        // Authorization header schemes, case-insensitive.
        (
            r#"curl -H "Authorization: Bot MTIzNDU2.abc.def" https://discord.com/api"#,
            r#"curl -H "Authorization: Bot ***" https://discord.com/api"#,
        ),
        ("curl -H 'authorization: bearer abc123' x", "curl -H 'authorization: bearer ***' x"),
        ("AUTHORIZATION: Basic dXNlcjpwYXNz", "AUTHORIZATION: Basic ***"),
        ("Authorization: token deadbeef", "Authorization: token ***"),
        ("Authorization: rawsecret", "Authorization: ***"),
        // Token prefixes.
        ("echo ghp_abcdefghijklmnopqrstuvwxyz0123456789", "echo ghp_***"),
        ("gho_ABCDEFGHIJKLMNOPQRSTUVWX ghu_ABCDEFGHIJKLMNOPQRSTUVWX", "gho_*** ghu_***"),
        ("ghs_ABCDEFGHIJKLMNOPQRSTUVWX ghr_ABCDEFGHIJKLMNOPQRSTUVWX", "ghs_*** ghr_***"),
        ("github_pat_11ABCDEFG0123456789_abcdefghijklmnop", "github_pat_***"),
        ("xoxb-1234567890-0987654321-AbCdEfGh xoxp-1234567890-x", "xoxb-*** xoxp-***"),
        ("xoxa-2-1234567890ab xapp-1-A0123-4567-abcdef", "xoxa-*** xapp-***"),
        ("key sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWx", "key sk-ant-***"),
        ("key sk-AbCdEfGhIjKlMnOpQrStUvWx", "key sk-***"),
        ("AKIAIOSFODNN7EXAMPLE and ASIAIOSFODNN7EXAMPLE", "AKIA*** and ASIA***"),
        ("?key=AIzaSyA-1234567890abcdefghijklmnopqrstu", "?key=AIza***"),
        (
            "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            "jwt eyJ***",
        ),
        // Telegram bot tokens, in an API path and bare.
        (
            "curl https://api.telegram.org/bot123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw/getMe",
            "curl https://api.telegram.org/bot123456789:***/getMe",
        ),
        ("echo 123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw", "echo 123456789:***"),
        // Secret-named env assignments: unquoted, double- and single-quoted.
        ("export GH_TOKEN=abc123 && gh pr list", "export GH_TOKEN=*** && gh pr list"),
        (r#"DB_PASSWORD="hunter2" ./run"#, r#"DB_PASSWORD="***" ./run"#),
        ("MY_API_KEY='x y z' AWS_SECRET_ACCESS_KEY=abc", "MY_API_KEY='***' AWS_SECRET_ACCESS_KEY=***"),
        ("apikey=1 private_key=2 credential=3 passwd=4", "apikey=*** private_key=*** credential=*** passwd=***"),
        ("curl 'https://x/cb?access_token=abc&state=1'", "curl 'https://x/cb?access_token=***&state=1'"),
        // `$`-prefixed literals: ANSI-C quoting and parameter-expansion defaults do not reference
        // a secret, they contain one.
        ("export GH_TOKEN=$'realsecret123' && x", "export GH_TOKEN=$'***' && x"),
        ("TOKEN=${X:-realsecret123} ./run", "TOKEN=${***} ./run"),
        ("tool --api-key ${KEY:-realsecret123}", "tool --api-key ${***}"),
        (r#"PASSWORD="${X:-realsecret123}" ./run"#, r#"PASSWORD="***" ./run"#),
        ("Authorization: Bearer $'realsecret123'", "Authorization: Bearer $'***'"),
        // Secret-named header / JSON / YAML keys.
        ("curl -H 'X-Api-Key: abc123' x", "curl -H 'X-Api-Key: ***' x"),
        (r#"{"password":"hunter2","user":"bob"}"#, r#"{"password":"***","user":"bob"}"#),
        // Secret-named CLI flags.
        ("tool --token abc --verbose", "tool --token *** --verbose"),
        ("tool --password=hunter2 --api-key 'k' --client-secret \"s\"", "tool --password=*** --api-key '***' --client-secret \"***\""),
        // URL userinfo.
        ("git clone https://bob:hunter2@github.com/o/r", "git clone https://bob:***@github.com/o/r"),
        (
            "git push https://x-access-token:abc@github.com/o/r",
            "git push https://x-access-token:***@github.com/o/r",
        ),
        // PEM private keys.
        (
            "cat > k.pem <<EOF\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----\nEOF",
            "cat > k.pem <<EOF\n-----BEGIN RSA PRIVATE KEY-----***-----END RSA PRIVATE KEY-----\nEOF",
        ),
        ("-----BEGIN PRIVATE KEY-----\nMIIEow", "-----BEGIN PRIVATE KEY-----***-----END PRIVATE KEY-----"),
    ];

    /// Readable titles that must pass through untouched: `$`-references, flag-shaped "values",
    /// and ordinary commands that merely mention secret-ish words.
    const UNREDACTED_TITLES: &[&str] = &[
        r#"curl -H "Authorization: Bot $DISCORD_BOT_TOKEN" https://discord.com/api"#,
        "Authorization: Bearer ${GH_TOKEN}",
        "export GH_TOKEN=$(cat /run/secrets/gh)",
        r#"GH_TOKEN="$(gh auth token)" gh api user"#,
        r#"TOKEN="$SLACK_BOT_TOKEN" curl x"#,
        r#"TOKEN="${SLACK_BOT_TOKEN}" curl x"#,
        "SLACK_BOT_TOKEN=$(tr '\\0' '\\n' < /proc/1/environ)",
        "gh auth login --with-token < token.txt",
        "tool --token-file ./t --password-stdin",
        "tool --token --verbose",
        "Read crates/openab-gateway/src/token.rs",
        "Read /home/node/.openab/cronjob.toml",
        r#"grep -n "token" config.toml"#,
        "git show 1234567:crates/openab-gateway/src/adapters/acp_server.rs",
        "cargo test -p openab-gateway",
        "git clone https://github.com/brettchien/openab",
        "ls ~/.config/gh",
        "Terminal",
        "",
    ];

    #[test]
    fn redact_tool_title_patterns() {
        for (input, expected) in REDACTION_CASES {
            assert_eq!(redact_tool_title(input), *expected, "input: {input}");
        }
    }

    #[test]
    fn redact_tool_title_discord_bot_token() {
        // Assembled at runtime so the fixture does not trip push-protection secret scanning.
        let id = "MTUzMTg0MDQwMzMwMzgyOTU2Ng";
        let token = [id, "GaBcDe", "abcdefghijklmnopqrstuvwxyz0123456789"].join(".");
        let out = redact_tool_title(&format!("token {token} x"));
        assert_eq!(out, format!("token {id}.*** x"));
        assert_eq!(redact_tool_title(&out), out);
        // Ordinary dotted names are left alone.
        let plain = "Read MTUzMTg0MDQwMzMwMzgyOTU2Ng.rs";
        assert_eq!(redact_tool_title(plain), plain);
    }

    #[test]
    fn redact_tool_title_keeps_references_and_plain_titles() {
        for title in UNREDACTED_TITLES {
            assert_eq!(redact_tool_title(title), *title);
        }
    }

    #[test]
    fn redact_tool_title_is_idempotent() {
        let inputs = REDACTION_CASES
            .iter()
            .map(|(i, _)| *i)
            .chain(UNREDACTED_TITLES.iter().copied());
        for input in inputs {
            let once = redact_tool_title(input);
            assert_eq!(redact_tool_title(&once), once, "input: {input}");
        }
    }

    #[test]
    fn redact_tool_title_non_ascii_safe() {
        assert_eq!(
            redact_tool_title("密碼 PASSWORD=秘密值 完成"),
            "密碼 PASSWORD=*** 完成"
        );
        assert_eq!(
            redact_tool_title(
                "🔑 Authorization: Bearer ťøķęñ✓ — ghp_abcdefghijklmnopqrstuvwxyz 🎉"
            ),
            "🔑 Authorization: Bearer *** — ghp_*** 🎉"
        );
        assert_eq!(redact_tool_title("é".repeat(500).as_str()), "é".repeat(500));
        assert_eq!(redact_tool_title("TOKEN=é"), "TOKEN=***");
    }
}
