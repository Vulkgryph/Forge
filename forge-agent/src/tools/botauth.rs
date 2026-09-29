// SPDX-License-Identifier: Apache-2.0
//! Web Bot Auth — proving who the crawler is, instead of asking to be believed.
//!
//! Forge already says what it is: an honest user agent with a contact URL, and
//! no pretending to be a browser. The trouble with saying it is that anyone can
//! say it. A header is a claim, and a site that has been scraped by something
//! wearing a polite name has no reason to treat the next polite name
//! differently.
//!
//! This signs each request instead. The private key stays here, the public half
//! is published at a well-known URL on a domain the operator controls, and a
//! site can check the signature rather than weigh a hint. Same claim, now
//! falsifiable.
//!
//! It is the cryptographic form of the position this project already took by
//! refusing to spoof a browser: here is who is calling, verifiably, and here is
//! where to complain.
//!
//! Implements `draft-meunier-webbotauth-httpsig-protocol`, which builds on
//! RFC 9421 (HTTP Message Signatures). Cloudflare validates these at its edge
//! and folded them into its Verified Bots programme, so this is the route by
//! which a small, well-behaved crawler can be recognised without a contract
//! and without being large enough for anyone to have heard of it.
//!
//! **Ed25519 is not hand-rolled.** The house rule is to write it ourselves
//! where we reasonably can, and signature verification is the standing
//! exception: a subtly wrong curve implementation fails open and silently, and
//! `ring` is already linked in via rustls. Nothing new enters the tree.

use base64::Engine as _;
use sha2::{Digest, Sha256};

/// The process-wide signer, set once at startup from config.
///
/// Same shape as `auth::set_offline_mode`: the crawler is reached through
/// several call paths that have no business carrying configuration, and
/// threading a key through all of them to reach one HTTP client would be
/// worse than a value set once before anything can use it.
static SIGNER: std::sync::OnceLock<Option<std::sync::Arc<Signer>>> =
    std::sync::OnceLock::new();

/// Load the configured key, if there is one. Called once, at startup.
///
/// A key without a directory is refused rather than used: the signature would
/// be unverifiable, and an unverifiable signature is worse than none — it
/// looks like a failed forgery instead of an honest unsigned request.
pub fn configure(key_path: Option<&str>, directory: Option<&str>) {
    let loaded = match (key_path, directory) {
        (Some(path), Some(dir)) => match std::fs::read(path) {
            Ok(pem) => match pkcs8_from_pem(&pem) {
                Some(der) => match Signer::new(&der, dir) {
                    Ok(signer) => {
                        eprintln!(
                            "forge: signing crawler requests as {dir} (key {})",
                            signer.keyid()
                        );
                        Some(std::sync::Arc::new(signer))
                    }
                    Err(e) => {
                        eprintln!("forge: web_bot_auth_key unusable: {e}");
                        None
                    }
                },
                None => {
                    eprintln!("forge: web_bot_auth_key is not a PEM private key: {path}");
                    None
                }
            },
            Err(e) => {
                eprintln!("forge: cannot read web_bot_auth_key {path}: {e}");
                None
            }
        },
        (Some(_), None) => {
            eprintln!(
                "forge: web_bot_auth_key is set but web_bot_auth_directory is not — \
                 not signing, because a signature nobody can look up is worse than none"
            );
            None
        }
        _ => None,
    };
    let _ = SIGNER.set(loaded);
}

/// The configured signer, or `None` when requests go unsigned.
pub fn signer() -> Option<std::sync::Arc<Signer>> {
    SIGNER.get().cloned().flatten()
}

/// The DER between the PEM armour. Written here rather than pulled in,
/// because it is base64 between two known lines.
pub fn pkcs8_from_pem(pem: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(pem).ok()?;
    let body: String = text
        .lines()
        .skip_while(|l| !l.starts_with("-----BEGIN"))
        .skip(1)
        .take_while(|l| !l.starts_with("-----END"))
        .collect();
    if body.is_empty() {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(body.trim()).ok()
}

/// The signature label used for Forge's own signature.
///
/// One signature, one label. The draft allows several — an agent and a remote
/// browser each signing — and Forge has no use for that yet.
const LABEL: &str = "sig1";

/// Required by the draft; a verifier uses it to tell this apart from other
/// uses of HTTP Message Signatures on the same request.
const TAG: &str = "web-bot-auth";

/// How long a signature stays valid.
///
/// The draft recommends no more than 24 hours. Five minutes is used instead:
/// the signature is generated per request and travels immediately, so a long
/// window buys nothing and a captured request stays replayable for the whole
/// of it.
const VALIDITY_SECS: u64 = 300;

/// A key that can sign outbound requests, and the URL where its public half
/// is published.
pub struct Signer {
    key: ring::signature::Ed25519KeyPair,
    /// base64url JWK SHA-256 thumbprint, per RFC 8037 A.3. A verifier uses
    /// this to pick the right key out of the published directory.
    keyid: String,
    /// The origin serving `/.well-known/http-message-signatures-directory`.
    directory: String,
}

/// The three headers a signed request carries.
pub struct Signed {
    pub signature_agent: String,
    pub signature_input: String,
    pub signature: String,
}

impl Signer {
    /// Load a PKCS#8 Ed25519 key and the directory URL that publishes it.
    ///
    /// `directory` is an origin, not a path — the well-known suffix is the
    /// verifier's business, and a signer that names a full path invites the
    /// two to disagree.
    pub fn new(pkcs8: &[u8], directory: &str) -> Result<Self, String> {
        // `maybe_unchecked` because PKCS#8 comes in two shapes: v2 carries the
        // public key alongside the private one, v1 carries only the private
        // key and the public half is derived. `from_pkcs8` accepts v2 alone,
        // and v1 is what `openssl genpkey` writes and what RFC 9421's own
        // test key is — so the strict form rejects the most ordinary input
        // there is.
        let key = ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(pkcs8)
            .map_err(|_| "not a PKCS#8 Ed25519 private key".to_string())?;
        let public = {
            use ring::signature::KeyPair as _;
            key.public_key().as_ref().to_vec()
        };
        Ok(Signer {
            keyid: jwk_thumbprint(&public),
            key,
            directory: directory.trim_end_matches('/').to_string(),
        })
    }

    pub fn keyid(&self) -> &str {
        &self.keyid
    }

    /// The JSON to publish at `/.well-known/http-message-signatures-directory`.
    ///
    /// Emitted by the signer rather than written by hand, because a directory
    /// that disagrees with the key is a signature nobody can verify and the
    /// failure is invisible from this side.
    pub fn directory_json(&self) -> String {
        use ring::signature::KeyPair as _;
        let x = b64url(self.key.public_key().as_ref());
        format!(
            "{{\"keys\":[{{\"kty\":\"OKP\",\"crv\":\"Ed25519\",\"alg\":\"Ed25519\",\
             \"use\":\"sig\",\"kid\":\"{}\",\"x\":\"{}\"}}]}}",
            self.keyid, x
        )
    }

    /// Sign a request to `authority` — the host, with a port when it is not
    /// the default for the scheme.
    ///
    /// `now` and `nonce` are arguments rather than read here so the whole
    /// thing can be checked against the draft's published test vectors, which
    /// fix both.
    pub fn sign(&self, authority: &str, now: u64, nonce: &[u8]) -> Signed {
        let agent_value = format!("{LABEL}=\"{}\"", self.directory);
        let params = format!(
            "(\"@authority\" \"signature-agent\";key=\"{LABEL}\")\
             ;created={now};keyid=\"{}\";alg=\"ed25519\";expires={}\
             ;nonce=\"{}\";tag=\"{TAG}\"",
            self.keyid,
            now + VALIDITY_SECS,
            base64::engine::general_purpose::STANDARD.encode(nonce),
        );
        let base = signature_base(
            &[
                ("\"@authority\"".to_string(), authority.to_string()),
                (
                    format!("\"signature-agent\";key=\"{LABEL}\""),
                    format!("\"{}\"", self.directory),
                ),
            ],
            &params,
        );
        let sig = self.key.sign(base.as_bytes());
        Signed {
            signature_agent: agent_value,
            signature_input: format!("{LABEL}={params}"),
            signature: format!(
                "{LABEL}=:{}:",
                base64::engine::general_purpose::STANDARD.encode(sig.as_ref())
            ),
        }
    }
}

/// The signature base of RFC 9421 §2.5: one line per covered component, then
/// `@signature-params`, joined by newlines with **no trailing newline**.
///
/// The absent trailing newline is the whole of it. A base that ends in one
/// produces a signature that verifies against nothing, and the only symptom
/// is a verifier silently declining to recognise the crawler — which is
/// indistinguishable from not having signed at all.
fn signature_base(components: &[(String, String)], params: &str) -> String {
    let mut lines: Vec<String> = components
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect();
    lines.push(format!("\"@signature-params\": {params}"));
    lines.join("\n")
}

/// RFC 8037 Appendix A.3: SHA-256 over the canonical JWK — required members
/// only, lexicographic, no whitespace — base64url without padding.
fn jwk_thumbprint(public_key: &[u8]) -> String {
    // Constructed by hand rather than serialised, because the canonical form
    // is defined by exact bytes and a serialiser is free to reorder or space
    // them however it likes.
    let json = format!(
        "{{\"crv\":\"Ed25519\",\"kty\":\"OKP\",\"x\":\"{}\"}}",
        b64url(public_key)
    );
    b64url(&Sha256::digest(json.as_bytes()))
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Ed25519 key from RFC 9421 Appendix B.1.4, which the Web Bot Auth
    /// draft's test vectors are signed with.
    const TEST_KEY_PKCS8_B64: &str =
        "MC4CAQAwBQYDK2VwBCIEIJ+DYvh6SEqVTm50DFtMDoQikTmiCqirVv9mWG9qfSnF";

    fn test_signer(directory: &str) -> Signer {
        let pkcs8 = base64::engine::general_purpose::STANDARD
            .decode(TEST_KEY_PKCS8_B64)
            .expect("the RFC's key decodes");
        Signer::new(&pkcs8, directory).expect("and loads")
    }

    /// The keyid is a JWK thumbprint, and the draft publishes what it should
    /// be for this key. If this is wrong, a verifier looks up the wrong key
    /// and every signature fails for a reason nothing reports.
    #[test]
    fn the_keyid_matches_the_drafts_published_thumbprint() {
        let signer = test_signer("https://signature-agent.test");
        assert_eq!(signer.keyid(), "poqkLGiymh_W0uP6PZFw-dvez3QJT5SolqXBCW38r0U");
    }

    /// Appendix C.2.2 of the draft, verbatim: the signature base with a
    /// `Signature-Agent` present.
    ///
    /// Checked as a string rather than by verifying our own signature,
    /// because a self-consistent implementation of the wrong base agrees with
    /// itself perfectly and is rejected by everybody else.
    #[test]
    fn the_signature_base_is_byte_for_byte_the_drafts_example() {
        let nonce = "n9p433xm+NJ3ph3upfBIGmsuwHw387YV7Q/F+6BSpGCVjYCqQw6rznNA8PVVLySrAWsv0hQtFioQb6E1YsauiA==";
        let params = format!(
            "(\"@authority\" \"signature-agent\";key=\"agent2\")\
             ;created=1735689600\
             ;keyid=\"poqkLGiymh_W0uP6PZFw-dvez3QJT5SolqXBCW38r0U\"\
             ;alg=\"ed25519\";expires=4889289600;nonce=\"{nonce}\";tag=\"web-bot-auth\""
        );
        let base = signature_base(
            &[
                ("\"@authority\"".to_string(), "example.com".to_string()),
                (
                    "\"signature-agent\";key=\"agent2\"".to_string(),
                    "\"https://signature-agent.test\"".to_string(),
                ),
            ],
            &params,
        );

        let expected = concat!(
            "\"@authority\": example.com\n",
            "\"signature-agent\";key=\"agent2\": \"https://signature-agent.test\"\n",
            "\"@signature-params\": (\"@authority\" \"signature-agent\";key=\"agent2\")",
            ";created=1735689600",
            ";keyid=\"poqkLGiymh_W0uP6PZFw-dvez3QJT5SolqXBCW38r0U\"",
            ";alg=\"ed25519\";expires=4889289600",
            ";nonce=\"n9p433xm+NJ3ph3upfBIGmsuwHw387YV7Q/F+6BSpGCVjYCqQw6rznNA8PVVLySrAWsv0hQtFioQb6E1YsauiA==\"",
            ";tag=\"web-bot-auth\""
        );
        assert_eq!(base, expected, "the signature base diverges from the draft");
        assert!(!base.ends_with('\n'), "a trailing newline breaks every verifier");
    }

    /// And the signature over that base is the one the draft publishes.
    ///
    /// This is the end-to-end check: our base construction, our key loading,
    /// and `ring`'s Ed25519 together have to reproduce a value computed by
    /// somebody else's implementation.
    #[test]
    fn the_signature_matches_the_drafts_test_vector() {
        let signer = test_signer("https://signature-agent.test");
        let nonce = "n9p433xm+NJ3ph3upfBIGmsuwHw387YV7Q/F+6BSpGCVjYCqQw6rznNA8PVVLySrAWsv0hQtFioQb6E1YsauiA==";
        let params = format!(
            "(\"@authority\" \"signature-agent\";key=\"agent2\")\
             ;created=1735689600\
             ;keyid=\"poqkLGiymh_W0uP6PZFw-dvez3QJT5SolqXBCW38r0U\"\
             ;alg=\"ed25519\";expires=4889289600;nonce=\"{nonce}\";tag=\"web-bot-auth\""
        );
        let base = signature_base(
            &[
                ("\"@authority\"".to_string(), "example.com".to_string()),
                (
                    "\"signature-agent\";key=\"agent2\"".to_string(),
                    "\"https://signature-agent.test\"".to_string(),
                ),
            ],
            &params,
        );
        let sig = signer.key.sign(base.as_bytes());
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(sig.as_ref()),
            "RdNFx5Bj6au3YgAMQL/RzmUlZE8QZLIaXGRpw985hWnwPfMxT228NMk6ehRS1PSl4e8PhbNZACSanGdhEwYCCg==",
            "the signature does not match the draft's vector",
        );
    }

    /// What Forge actually sends, as opposed to what the draft's examples do.
    #[test]
    fn a_signed_request_carries_the_three_headers_the_draft_requires() {
        let signer = test_signer("https://vulkgryph.com/");
        let out = signer.sign("docs.example", 1_800_000_000, &[7u8; 64]);

        assert_eq!(out.signature_agent, "sig1=\"https://vulkgryph.com\"",
                   "the trailing slash was not trimmed");
        for required in ["created=1800000000", "expires=1800000300",
                         "tag=\"web-bot-auth\"", "alg=\"ed25519\"", "nonce="] {
            assert!(out.signature_input.contains(required),
                    "missing {required}: {}", out.signature_input);
        }
        assert!(out.signature_input.contains(signer.keyid()));
        // The signature is a byte sequence in structured-field form.
        assert!(out.signature.starts_with("sig1=:") && out.signature.ends_with(':'));
    }

    /// The published directory has to describe the key doing the signing.
    #[test]
    fn the_directory_describes_the_signing_key() {
        let signer = test_signer("https://vulkgryph.com");
        let dir = signer.directory_json();
        assert!(dir.contains("\"kty\":\"OKP\""), "{dir}");
        assert!(dir.contains("\"crv\":\"Ed25519\""), "{dir}");
        assert!(dir.contains(signer.keyid()), "the kid is not the thumbprint: {dir}");
        // The public half, and only the public half.
        assert!(!dir.contains("\"d\""), "a private key was about to be published");
    }

    /// The PEM reader is hand-written, so it gets its own check against the
    /// RFC's own armoured key.
    #[test]
    fn a_pem_private_key_is_unwrapped_to_der() {
        let pem = b"-----BEGIN PRIVATE KEY-----\n\
                    MC4CAQAwBQYDK2VwBCIEIJ+DYvh6SEqVTm50DFtMDoQikTmiCqirVv9mWG9qfSnF\n\
                    -----END PRIVATE KEY-----\n";
        let der = pkcs8_from_pem(pem).expect("the RFC's armoured key unwraps");
        let signer = Signer::new(&der, "https://vulkgryph.com").expect("and loads");
        assert_eq!(signer.keyid(), "poqkLGiymh_W0uP6PZFw-dvez3QJT5SolqXBCW38r0U");

        // Not PEM at all, and PEM with nothing in it.
        assert!(pkcs8_from_pem(b"just some bytes").is_none());
        assert!(pkcs8_from_pem(b"-----BEGIN PRIVATE KEY-----\n-----END PRIVATE KEY-----\n").is_none());
    }

    /// What a verifier does, done here: take only the emitted headers,
    /// rebuild the signature base from them, and check the signature with the
    /// published public key.
    ///
    /// The test-vector checks above prove the base matches the draft. This
    /// proves the *headers* match the base — that what Forge sends is what
    /// Forge signed. Those can drift apart independently, and if they do the
    /// only symptom is a site quietly declining to recognise the crawler.
    #[test]
    fn a_site_can_verify_what_forge_actually_sends() {
        use ring::signature::KeyPair as _;

        let signer = test_signer("https://vulkgryph.com");
        let out = signer.sign("docs.rs", 1_800_000_000, &[3u8; 64]);

        // Reconstructed the way a verifier must: from the wire, not from any
        // value this module kept around.
        let params = out
            .signature_input
            .strip_prefix("sig1=")
            .expect("the label is where the draft puts it");
        let agent_value = out
            .signature_agent
            .strip_prefix("sig1=")
            .expect("the Signature-Agent member carries the same label");
        let rebuilt = format!(
            "\"@authority\": docs.rs\n\
             \"signature-agent\";key=\"sig1\": {agent_value}\n\
             \"@signature-params\": {params}"
        );

        let sig_b64 = out
            .signature
            .strip_prefix("sig1=:")
            .and_then(|r| r.strip_suffix(':'))
            .expect("a byte sequence in structured-field form");
        let sig = base64::engine::general_purpose::STANDARD
            .decode(sig_b64)
            .expect("base64");

        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::ED25519,
            signer.key.public_key().as_ref(),
        );
        public
            .verify(rebuilt.as_bytes(), &sig)
            .expect("a verifier rebuilding the base from the headers must succeed");

        // And it must fail when the request differs from what was signed —
        // otherwise the signature is decoration.
        let tampered = rebuilt.replace("docs.rs", "evil.example");
        assert!(
            public.verify(tampered.as_bytes(), &sig).is_err(),
            "the signature verified against a different authority"
        );
    }

    #[test]
    fn a_key_that_is_not_ed25519_is_refused_rather_than_guessed_at() {
        assert!(Signer::new(b"not a key at all", "https://vulkgryph.com").is_err());
        assert!(Signer::new(&[], "https://vulkgryph.com").is_err());
    }
}
