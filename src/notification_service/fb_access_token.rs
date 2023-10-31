// copied from https://crates.io/crates/gauth, but
// modified to let it work with tokio

use chrono::Utc;

use serde_derive;

use super::{errors::ServiceAccountError, jwt};

#[derive(Debug, Clone)]
pub struct ServiceAccount {
    scopes: String,
    key_path: String,
    user_email: Option<String>,

    access_token: Option<String>,
    expires_at: Option<u64>,

    http_client: reqwest::Client,
}

#[derive(Debug, serde_derive::Deserialize)]
struct Token {
    access_token: String,
    expires_in: u64,
    token_type: String,
}

impl Token {
    fn bearer_token(&self) -> String {
        format!("{} {}", self.token_type, self.access_token)
    }
}

impl ServiceAccount {
    /// Creates a new service account from a key file and scopes
    pub fn from_file(key_path: &str, scopes: Vec<&str>) -> Self {
        Self {
            scopes: scopes.join(" "),
            key_path: key_path.to_string(),
            user_email: None,

            access_token: None,
            expires_at: None,

            http_client: reqwest::Client::new(),
        }
    }

    /// Returns an access token
    /// If the access token is not expired, it will return the cached access token
    /// Otherwise, it will exchange the JWT token for an access token
    pub async fn access_token(&mut self) -> Result<String, ()> {
        match (self.access_token.as_ref(), self.expires_at) {
            (Some(access_token), Some(expires_at))
                if expires_at > Utc::now().timestamp() as u64 =>
            {
                Ok(access_token.to_string())
            }
            _ => {
                let jwt_token = self.jwt_token()?;
                let token = match self.exchange_jwt_token_for_access_token(jwt_token).await {
                    Ok(token) => token,
                    Err(_) => return Err(()),
                };

                let expires_at = Utc::now().timestamp() as u64 + token.expires_in - 30;

                self.access_token = Some(token.bearer_token());
                self.expires_at = Some(expires_at);

                Ok(token.bearer_token())
            }
        }
    }

    async fn exchange_jwt_token_for_access_token(
        &mut self,
        jwt_token: jwt::JwtToken,
    ) -> Result<Token, ServiceAccountError> {
        let jwt_token_str = &jwt_token.to_string()?;

        let resp = self
            .http_client
            .post(jwt_token.token_uri())
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", jwt_token_str),
            ])
            .send()
            .await;

        let resp = match resp {
            Ok(resp) => resp,
            Err(err) => {
                println!("err: {:?}", err);
                return Err(ServiceAccountError::HttpResponse(err.to_string()));
            }
        };

        let resp_bytes = match resp.bytes().await {
            Ok(resp_bytes) => resp_bytes,
            Err(err) => {
                println!("err: {:?}", err);
                return Err(ServiceAccountError::HttpResponse(err.to_string()));
            }
        };

        let resp_json = match String::from_utf8(resp_bytes.to_vec()) {
            Ok(resp_json) => resp_json,
            Err(err) => {
                println!("err: {:?}", err);
                return Err(ServiceAccountError::HttpResponse(err.to_string()));
            }
        };

        let token = match serde_json::from_str::<Token>(&resp_json) {
            Ok(token) => token,
            Err(err) => {
                println!("err: {:?}", err);
                return Err(ServiceAccountError::SerdeJson(err));
            }
        };

        Ok(token)
    }

    fn jwt_token(&self) -> Result<jwt::JwtToken, ()> {
        let token = jwt::JwtToken::from_file(&self.key_path).unwrap();

        Ok(match self.user_email {
            Some(ref user_email) => token.sub(user_email.to_string()),
            None => token,
        }
        .scope(self.scopes.clone()))
    }
}
