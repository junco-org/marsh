mod output;
mod preset;
mod runner;

use rmux_proto::RmuxError;

use super::settings::WebShareSettings;
use crate::io::ShellIo;

pub(crate) use runner::TunnelHandle;

#[derive(Debug)]
pub(crate) struct TunnelInfo {
    pub(crate) handle: TunnelHandle,
    pub(crate) provider: String,
    pub(crate) public_url: String,
}

/// Starts the tunnel provider named `name`.
///
/// # Errors
///
/// Fails when no such preset exists, when reading one was refused, and when the provider did not
/// reach a working public URL.
pub(crate) async fn start_provider(
    io: &ShellIo,
    name: &str,
    settings: &WebShareSettings,
) -> Result<TunnelInfo, RmuxError> {
    let preset = preset::load(io, name).await?;
    runner::start(io, preset, settings).await
}

#[cfg(test)]
mod tests {
    use super::preset::{available_from, parse, PresetSource};

    #[test]
    fn embedded_presets_are_valid() {
        for (name, content) in super::preset::embedded() {
            parse(name, PresetSource::Embedded, content).expect("embedded preset parses");
        }
    }

    #[test]
    fn available_presets_merge_sorted_unique_and_legal() {
        let names = available_from(
            ["b".to_owned(), "a".to_owned()],
            [
                "b".to_owned(),
                "not a preset name".to_owned(),
                "c".to_owned(),
            ],
        );
        assert_eq!(names, vec!["a", "b", "c"]);
    }
}
