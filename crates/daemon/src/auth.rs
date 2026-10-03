//! Public-client authorization. Secrets in this module must never be logged.

use oauth2::basic::{BasicClient, BasicTokenResponse};
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, RefreshToken, Scope, TokenUrl,
};
use sha2::{Digest, Sha256};
use url::Url;

const AUTH_ENDPOINT: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize";
const TOKEN_ENDPOINT: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";

#[derive(Debug, PartialEq, Eq)]
pub enum CallbackError {
    InvalidUrl,
    InvalidPath,
    MissingState,
    InvalidState,
    MissingCode,
    InvalidCode,
    Denied,
}

pub struct LoginAttempt {
    client_id: String,
    redirect_url: String,
    csrf: CsrfToken,
    verifier: PkceCodeVerifier,
}

pub async fn refresh_token(
    client_id: &str,
    refresh: &str,
) -> Result<BasicTokenResponse, Box<dyn std::error::Error + Send + Sync>> {
    let client = BasicClient::new(ClientId::new(client_id.to_owned()))
        .set_token_uri(TokenUrl::new(TOKEN_ENDPOINT.to_owned())?);
    let http = oauth2::reqwest::ClientBuilder::new()
        .redirect(oauth2::reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    Ok(client
        .exchange_refresh_token(&RefreshToken::new(refresh.to_owned()))
        .request_async(&http)
        .await?)
}

impl LoginAttempt {
    pub fn begin(client_id: &str, port: u16) -> Result<(Self, Url), url::ParseError> {
        let redirect_url = format!("http://localhost:{port}/callback");
        let client = BasicClient::new(ClientId::new(client_id.to_owned()))
            .set_auth_uri(AuthUrl::new(AUTH_ENDPOINT.to_owned())?)
            .set_token_uri(TokenUrl::new(TOKEN_ENDPOINT.to_owned())?)
            .set_redirect_uri(RedirectUrl::new(redirect_url.clone())?);
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (auth_url, csrf) = client
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new("Files.ReadWrite".to_owned()))
            .add_scope(Scope::new("offline_access".to_owned()))
            .set_pkce_challenge(challenge)
            .url();
        Ok((
            Self {
                client_id: client_id.to_owned(),
                redirect_url,
                csrf,
                verifier,
            },
            auth_url,
        ))
    }

    pub fn parse_callback(&self, target: &str) -> Result<String, CallbackError> {
        let base = Url::parse(&self.redirect_url).map_err(|_| CallbackError::InvalidUrl)?;
        let callback = base.join(target).map_err(|_| CallbackError::InvalidUrl)?;
        if callback.scheme() != base.scheme()
            || callback.host_str() != base.host_str()
            || callback.port_or_known_default() != base.port_or_known_default()
        {
            return Err(CallbackError::InvalidUrl);
        }
        if callback.path() != "/callback" {
            return Err(CallbackError::InvalidPath);
        }
        let state = callback
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .ok_or(CallbackError::MissingState)?;
        if callback
            .query_pairs()
            .filter(|(key, _)| key == "state")
            .count()
            != 1
        {
            return Err(CallbackError::InvalidState);
        }
        if Sha256::digest(state.as_bytes()) != Sha256::digest(self.csrf.secret().as_bytes()) {
            return Err(CallbackError::InvalidState);
        }
        if callback.query_pairs().any(|(key, _)| key == "error") {
            return Err(CallbackError::Denied);
        }
        let mut codes = callback.query_pairs().filter(|(key, _)| key == "code");
        let code = codes
            .next()
            .map(|(_, value)| value.into_owned())
            .filter(|value| !value.is_empty())
            .ok_or(CallbackError::MissingCode)?;
        if codes.next().is_some() {
            return Err(CallbackError::InvalidCode);
        }
        Ok(code)
    }

    pub async fn exchange_code(
        self,
        code: String,
    ) -> Result<BasicTokenResponse, Box<dyn std::error::Error + Send + Sync>> {
        let client = BasicClient::new(ClientId::new(self.client_id))
            .set_auth_uri(AuthUrl::new(AUTH_ENDPOINT.to_owned())?)
            .set_token_uri(TokenUrl::new(TOKEN_ENDPOINT.to_owned())?)
            .set_redirect_uri(RedirectUrl::new(self.redirect_url)?);
        let http = oauth2::reqwest::ClientBuilder::new()
            .redirect(oauth2::reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let token = client
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(self.verifier)
            .request_async(&http)
            .await?;
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_url_uses_pkce_and_minimal_scopes() {
        let (_, url) = LoginAttempt::begin("test-client", 45000).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.host_str(), Some("login.microsoftonline.com"));
        assert_eq!(
            query.get("client_id").map(String::as_str),
            Some("test-client")
        );
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some("http://localhost:45000/callback")
        );
        assert!(query.get("scope").unwrap().contains("Files.ReadWrite"));
        assert!(query.get("scope").unwrap().contains("offline_access"));
        assert!(!query.contains_key("client_secret"));
    }

    #[test]
    fn callback_rejects_wrong_state_and_origin() {
        let (attempt, url) = LoginAttempt::begin("test-client", 45000).unwrap();
        let state = url.query_pairs().find(|(key, _)| key == "state").unwrap().1;
        assert_eq!(
            attempt.parse_callback("/callback?state=wrong&code=abc"),
            Err(CallbackError::InvalidState)
        );
        assert_eq!(
            attempt.parse_callback(&format!(
                "http://attacker.invalid/callback?state={state}&code=abc"
            )),
            Err(CallbackError::InvalidUrl)
        );
        assert_eq!(
            attempt.parse_callback(&format!("/callback?state={state}&code=abc")),
            Ok("abc".to_owned())
        );
        assert_eq!(
            attempt.parse_callback(&format!("/callback?state={state}&state={state}&code=abc")),
            Err(CallbackError::InvalidState)
        );
        assert_eq!(
            attempt.parse_callback(&format!("/callback?state={state}&code=abc&code=def")),
            Err(CallbackError::InvalidCode)
        );
        assert_eq!(
            attempt.parse_callback(&format!("/callback?state={state}&error=access_denied")),
            Err(CallbackError::Denied)
        );
    }
}
