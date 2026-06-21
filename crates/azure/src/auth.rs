use sre_common::errors::AzureError;
use tracing::info;

/// Token credential abstraction for Azure API authentication.
///
/// In production, use `azure_identity::DefaultAzureCredential` which auto-selects:
/// - Managed Identity (when running in Azure ACA/AKS/VM)
/// - Azure CLI credential (local development)
/// - Environment variables (CI/CD)
///
/// We wrap this behind a trait to enable testing without Azure.
#[async_trait::async_trait]
pub trait TokenProvider: Send + Sync {
    /// Get an OAuth 2.0 bearer token for the given scope
    async fn get_token(&self, scope: &str) -> Result<String, AzureError>;
}

/// Azure credential provider using azure_identity DefaultAzureCredential
pub struct AzureCredentialProvider {
    // In the real implementation, this holds:
    // credential: azure_identity::DefaultAzureCredential
    //
    // For now, we use a placeholder that reads from environment
    // to avoid requiring azure_identity to compile without Azure SDK installed
    _private: (),
}

impl AzureCredentialProvider {
    pub fn new() -> Result<Self, AzureError> {
        info!("Initializing Azure credential provider (DefaultAzureCredential)");
        Ok(Self { _private: () })
    }
}

#[async_trait::async_trait]
impl TokenProvider for AzureCredentialProvider {
    async fn get_token(&self, scope: &str) -> Result<String, AzureError> {
        // Production implementation:
        // let token = self.credential
        //     .get_token(&[scope])
        //     .await
        //     .map_err(|e| AzureError::AuthFailed(e.to_string()))?;
        // Ok(token.token.secret().to_string())

        // Placeholder: read from AZURE_ACCESS_TOKEN env var for initial development
        std::env::var("AZURE_ACCESS_TOKEN").map_err(|_| {
            AzureError::AuthFailed(format!(
                "No credential available for scope {scope}. \
                 Set AZURE_ACCESS_TOKEN or configure Azure CLI auth."
            ))
        })
    }
}

/// Mock token provider for testing
pub struct MockTokenProvider {
    pub token: String,
}

impl MockTokenProvider {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

#[async_trait::async_trait]
impl TokenProvider for MockTokenProvider {
    async fn get_token(&self, _scope: &str) -> Result<String, AzureError> {
        Ok(self.token.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_provider_returns_its_token() {
        let provider = MockTokenProvider::new("tok-123");
        assert_eq!(provider.token, "tok-123");
        assert_eq!(provider.get_token("any-scope").await.unwrap(), "tok-123");
    }

    #[tokio::test]
    async fn azure_credential_provider_constructs() {
        assert!(AzureCredentialProvider::new().is_ok());
    }

    #[tokio::test]
    async fn azure_credential_provider_token_matches_env() {
        // Read (never mutate) the ambient env to stay race-free with the
        // reqwest-client-building tests that share this test binary. We assert
        // whichever branch of get_token corresponds to the current environment.
        let provider = AzureCredentialProvider::new().unwrap();
        let result = provider.get_token("https://management.azure.com/.default").await;
        match std::env::var("AZURE_ACCESS_TOKEN") {
            Ok(token) if !token.is_empty() => assert_eq!(result.unwrap(), token),
            _ => {
                let err = result.unwrap_err();
                assert!(matches!(err, AzureError::AuthFailed(_)));
            }
        }
    }
}
