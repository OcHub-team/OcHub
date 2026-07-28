use serde_json::{Value, json};

use crate::application::{Application, ApplicationError, ApplicationResult, redact_json};
use crate::deeplink::DeepLinkImportRequest;

impl Application {
    pub fn parse_deeplink(&self, uri: &str, show_secrets: bool) -> ApplicationResult<Value> {
        let request = crate::deeplink::parse_deeplink_url(uri)?;
        let value = serde_json::to_value(request)
            .map_err(|source| crate::AppError::JsonSerialize { source })?;
        Ok(if show_secrets {
            value
        } else {
            redact_json(&value)
        })
    }

    pub fn import_deeplink(&self, uri: &str) -> ApplicationResult<Value> {
        let request = crate::deeplink::parse_deeplink_url(uri)?;
        self.import_deeplink_request(request)
    }

    fn import_deeplink_request(&self, request: DeepLinkImportRequest) -> ApplicationResult<Value> {
        match request.resource.as_str() {
            "provider" => {
                let app = request.app.clone();
                let id = crate::deeplink::import_provider_from_deeplink(self.state(), request)?;
                Ok(json!({
                    "resource": "provider",
                    "app": app,
                    "id": id,
                    "imported": true
                }))
            }
            "mcp" => {
                let result = crate::deeplink::import_mcp_from_deeplink(self.state(), request)?;
                if !result.failed.is_empty() {
                    let details = serde_json::to_value(&result)
                        .map_err(|source| crate::AppError::JsonSerialize { source })?;
                    return Err(ApplicationError::PartialFailure {
                        message: "one or more MCP servers could not be imported".to_string(),
                        details,
                    });
                }
                serde_json::to_value(result)
                    .map_err(|source| crate::AppError::JsonSerialize { source }.into())
            }
            "skill" => {
                let id = crate::deeplink::import_skill_from_deeplink(self.state(), request)?;
                Ok(json!({
                    "resource": "skill-repository",
                    "id": id,
                    "imported": true
                }))
            }
            resource => Err(ApplicationError::InvalidInput(format!(
                "unsupported deep link resource: {resource}"
            ))),
        }
    }
}
