mod claude;
mod codex;

use crate::model::{Provider, ProviderSnapshot};
use crate::settings::ProviderSettings;

pub fn refresh(provider: Provider, config: &ProviderSettings) -> Result<ProviderSnapshot, String> {
    match provider {
        Provider::Claude => claude::refresh(config),
        Provider::Codex => codex::refresh(config),
    }
}
