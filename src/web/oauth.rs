use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde_json::Value;

use super::session::{random_token, sha256};
use super::settings::{GoogleEndpoints, WebSettings};
use crate::api::auth::Token;

/// A sign-in must come back from Google within this time.
pub const LOGIN_TTL_SECS: i64 = 600;
const MAX_PENDING_LOGINS: usize = 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OAuthError {
    #[error("too many sign-ins in progress; try again in a few minutes")]
    TooManyLogins,
    #[error("Google did not answer: {0}")]
    Request(String),
    #[error("Google refused the sign-in: {0}")]
    Rejected(String),
    #[error("invalid answer from Google: {0}")]
    Invalid(String),
    #[error("the sign-in does not match this request")]
    Mismatch,
    #[error("Google has not verified this email")]
    Unverified,
}

/// What `/auth/login` sends to Google. The verifier and the nonce stay on the server.
#[derive(Debug)]
pub struct Login {
    pub state: String,
    pub nonce: String,
    pub challenge: String,
}

/// What `/auth/callback` needs to finish a sign-in.
#[derive(Debug, PartialEq, Eq)]
pub struct Pending {
    pub verifier: String,
    pub nonce: String,
}

/// Sign-ins that went to Google and have not come back, keyed by `state`. Each is used once.
#[derive(Default)]
pub struct Logins {
    pending: Mutex<HashMap<String, (Pending, i64)>>,
}

impl Logins {
    pub fn start(&self, now: i64) -> Result<Login, OAuthError> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.retain(|_, (_, started)| now - *started < LOGIN_TTL_SECS);
        if pending.len() >= MAX_PENDING_LOGINS {
            return Err(OAuthError::TooManyLogins);
        }
        let verifier = random_token();
        let login = Login {
            state: random_token(),
            nonce: random_token(),
            challenge: sha256(&verifier),
        };
        let entry = Pending {
            verifier,
            nonce: login.nonce.clone(),
        };
        pending.insert(login.state.clone(), (entry, now));
        Ok(login)
    }

    /// Takes the sign-in of `state` out; `None` when it is unknown, used or too old.
    pub fn finish(&self, state: &str, now: i64) -> Option<Pending> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (entry, started) = pending.remove(state)?;
        (now - started < LOGIN_TTL_SECS).then_some(entry)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    id_token: String,
}

#[derive(Deserialize)]
struct UserInfo {
    sub: String,
    email: Option<String>,
    #[serde(default)]
    email_verified: bool,
}

/// Google sign-in with the authorization code flow and PKCE. The client secret is never logged.
pub struct Google {
    client: reqwest::Client,
    endpoints: GoogleEndpoints,
    client_id: String,
    client_secret: Token,
    redirect_uri: String,
}

impl Google {
    pub fn new(settings: &WebSettings, client_secret: Token) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("the HTTP client settings are valid");
        Self {
            client,
            endpoints: settings.google.clone(),
            client_id: settings.google_client_id.clone(),
            client_secret,
            redirect_uri: settings.redirect_uri(),
        }
    }

    /// Google's sign-in page for this login.
    pub fn authorize_url(&self, login: &Login) -> String {
        let query = [
            ("response_type", "code"),
            ("client_id", &self.client_id),
            ("redirect_uri", &self.redirect_uri),
            ("scope", "openid email"),
            ("state", &login.state),
            ("nonce", &login.nonce),
            ("code_challenge", &login.challenge),
            ("code_challenge_method", "S256"),
            ("prompt", "select_account"),
        ];
        reqwest::Url::parse_with_params(&self.endpoints.auth_url, query)
            .map(String::from)
            .unwrap_or_else(|_| self.endpoints.auth_url.clone())
    }

    /// Exchanges the code and returns the verified email of the user.
    pub async fn email(&self, code: &str, pending: &Pending) -> Result<String, OAuthError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", &self.client_id),
            ("client_secret", self.client_secret.as_str()),
            ("redirect_uri", &self.redirect_uri),
            ("code_verifier", &pending.verifier),
        ];
        let request = self.client.post(&self.endpoints.token_url).form(&form);
        let tokens: TokenResponse = send(request).await?;
        let subject = self.check_id_token(&tokens.id_token, &pending.nonce)?;

        let request = self
            .client
            .get(&self.endpoints.userinfo_url)
            .bearer_auth(&tokens.access_token);
        let info: UserInfo = send(request).await?;
        if info.sub != subject {
            return Err(OAuthError::Mismatch);
        }
        match info.email {
            Some(email) if info.email_verified => Ok(email),
            _ => Err(OAuthError::Unverified),
        }
    }

    /// Checks the nonce and audience of the ID token and returns its subject.
    /// The token came straight from Google's token endpoint over TLS, so its signature is not checked.
    fn check_id_token(&self, id_token: &str, nonce: &str) -> Result<String, OAuthError> {
        let invalid = |reason: &str| OAuthError::Invalid(format!("ID token: {reason}"));
        let payload = id_token
            .split('.')
            .nth(1)
            .ok_or_else(|| invalid("not a JWT"))?;
        let bytes = URL_SAFE_NO_PAD
            .decode(payload.trim_end_matches('='))
            .map_err(|_| invalid("not base64"))?;
        let claims: Value = serde_json::from_slice(&bytes).map_err(|_| invalid("not JSON"))?;

        let audience_matches = match &claims["aud"] {
            Value::String(audience) => *audience == self.client_id,
            Value::Array(audiences) => audiences.iter().any(|one| *one == *self.client_id),
            _ => false,
        };
        if !audience_matches || claims["nonce"].as_str() != Some(nonce) {
            return Err(OAuthError::Mismatch);
        }
        claims["sub"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid("no subject"))
    }
}

/// Sends a request and reads its JSON body. Errors carry no URL, code or token.
async fn send<T: serde::de::DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Result<T, OAuthError> {
    let response = request
        .send()
        .await
        .map_err(|error| OAuthError::Request(error.without_url().to_string()))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|error| OAuthError::Invalid(error.without_url().to_string()))?;
    if !status.is_success() {
        let reason = body["error"].as_str().unwrap_or("no reason given");
        return Err(OAuthError::Rejected(format!("{status}: {reason}")));
    }
    serde_json::from_value(body).map_err(|error| OAuthError::Invalid(error.to_string()))
}

#[cfg(test)]
pub(super) mod tests {
    use std::net::SocketAddr;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    use axum::{Form, Json, Router};
    use serde_json::json;

    use super::*;

    /// A fake Google that accepts `good-code` for the expected login.
    #[derive(Clone)]
    pub struct FakeGoogle {
        /// The email it reports and whether it is verified.
        pub email: Arc<Mutex<(String, bool)>>,
        /// The nonce and PKCE challenge of the expected login.
        expected: Arc<Mutex<(String, String)>>,
    }

    async fn token(
        State(fake): State<FakeGoogle>,
        Form(form): Form<HashMap<String, String>>,
    ) -> (axum::http::StatusCode, Json<Value>) {
        let (nonce, challenge) = fake.expected.lock().unwrap().clone();
        let field = |name: &str| form.get(name).map(String::as_str);
        let valid = field("code") == Some("good-code")
            && field("client_secret") == Some("google-secret")
            && field("code_verifier").map(sha256) == Some(challenge);
        if !valid {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid_grant"})),
            );
        }
        let claims = json!({"aud": "client-id", "nonce": nonce, "sub": "user-1"});
        let id_token = format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()));
        let body = json!({"access_token": "access-1", "id_token": id_token});
        (axum::http::StatusCode::OK, Json(body))
    }

    async fn userinfo(State(fake): State<FakeGoogle>, headers: HeaderMap) -> Json<Value> {
        assert_eq!(headers["authorization"], "Bearer access-1");
        let (email, verified) = fake.email.lock().unwrap().clone();
        Json(json!({"sub": "user-1", "email": email, "email_verified": verified}))
    }

    impl FakeGoogle {
        /// Starts the fake and returns it with its address.
        pub async fn start() -> (Self, SocketAddr) {
            let fake = Self {
                email: Arc::new(Mutex::new(("owner@example.com".into(), true))),
                expected: Arc::default(),
            };
            let router = Router::new()
                .route("/token", post(token))
                .route("/userinfo", get(userinfo))
                .with_state(fake.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move { axum::serve(listener, router).await });
            (fake, address)
        }

        /// The fake answers only for this login: its nonce and PKCE challenge.
        pub fn expect(&self, login: &Login) {
            *self.expected.lock().unwrap() = (login.nonce.clone(), login.challenge.clone());
        }
    }

    pub fn settings_text(google: SocketAddr, secret_file: &std::path::Path) -> String {
        format!(
            "listen = \"127.0.0.1:0\"\npublic_url = \"https://upkeep.example.com\"\n\
             google_client_id = \"client-id\"\ngoogle_client_secret_file = \"{}\"\n\
             [google]\nauth_url = \"https://accounts.example.com/auth\"\n\
             token_url = \"http://{google}/token\"\nuserinfo_url = \"http://{google}/userinfo\"\n",
            secret_file.display()
        )
    }

    async fn google() -> (FakeGoogle, Google, tempfile::TempDir) {
        let (fake, address) = FakeGoogle::start().await;
        let dir = tempfile::tempdir().unwrap();
        let secret_file = dir.path().join("secret");
        std::fs::write(&secret_file, "google-secret\n").unwrap();
        std::fs::set_permissions(&secret_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let text = settings_text(address, &secret_file)
            + "[hosts.a]\nlabel = \"A\"\nurl = \"http://127.0.0.1:1\"\ntoken_file = \"/t\"\n";
        let settings: WebSettings = toml::from_str(&text).unwrap();
        let google = Google::new(&settings, Token::load(&secret_file).unwrap());
        (fake, google, dir)
    }

    #[test]
    fn pkce_challenge_is_s256_of_the_verifier() {
        // RFC 7636, appendix B.
        assert_eq!(
            sha256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn state_is_single_use_and_expires() {
        let logins = Logins::default();
        let login = logins.start(1000).unwrap();
        let pending = logins.finish(&login.state, 1001).unwrap();
        assert_eq!(pending.nonce, login.nonce);
        assert_eq!(sha256(&pending.verifier), login.challenge);
        assert!(logins.finish(&login.state, 1002).is_none());

        let late = logins.start(1000).unwrap();
        assert!(logins.finish(&late.state, 1000 + LOGIN_TTL_SECS).is_none());
        assert!(logins.finish("made-up", 1000).is_none());
    }

    #[test]
    fn pending_logins_are_capped() {
        let logins = Logins::default();
        for _ in 0..MAX_PENDING_LOGINS {
            logins.start(1000).unwrap();
        }
        assert_eq!(logins.start(1000).unwrap_err(), OAuthError::TooManyLogins);
        assert!(logins.start(1000 + LOGIN_TTL_SECS).is_ok());
    }

    #[tokio::test]
    async fn authorize_url_carries_state_nonce_and_challenge() {
        let (_fake, google, _dir) = google().await;
        let login = Logins::default().start(0).unwrap();
        let url = reqwest::Url::parse(&google.authorize_url(&login)).unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["state"], login.state);
        assert_eq!(query["nonce"], login.nonce);
        assert_eq!(query["code_challenge"], login.challenge);
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(
            query["redirect_uri"],
            "https://upkeep.example.com/auth/callback"
        );
        assert!(!url.as_str().contains("google-secret"));
    }

    #[tokio::test]
    async fn exchange_returns_the_verified_email() {
        let (fake, google, _dir) = google().await;
        let logins = Logins::default();
        let login = logins.start(0).unwrap();
        fake.expect(&login);
        let pending = logins.finish(&login.state, 1).unwrap();

        assert_eq!(
            google.email("good-code", &pending).await.unwrap(),
            "owner@example.com"
        );
        assert!(matches!(
            google.email("bad-code", &pending).await,
            Err(OAuthError::Rejected(reason)) if reason.contains("invalid_grant")
        ));

        *fake.email.lock().unwrap() = ("owner@example.com".into(), false);
        assert_eq!(
            google.email("good-code", &pending).await,
            Err(OAuthError::Unverified)
        );
    }

    #[tokio::test]
    async fn a_wrong_nonce_is_refused() {
        let (fake, google, _dir) = google().await;
        let logins = Logins::default();
        let login = logins.start(0).unwrap();
        fake.expect(&login);
        let mut pending = logins.finish(&login.state, 1).unwrap();
        pending.nonce = "another-nonce".into();
        assert_eq!(
            google.email("good-code", &pending).await,
            Err(OAuthError::Mismatch)
        );
    }
}
