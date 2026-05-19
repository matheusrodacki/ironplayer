/// SPEC-CFG-001
/// Configuração da aplicação carregada de `ironstream.toml` na pasta do executável.
/// Valores padrão sempre definidos via `Default`. Nunca falha no startup.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Sub-structs
// ---------------------------------------------------------------------------

/// Configurações de rede.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct NetworkConfig {
    /// Tamanho do buffer UDP em bytes. Padrão: 4 MB.
    pub udp_buffer_bytes: usize,
    /// Timeout de recepção em milissegundos. Padrão: 5 000.
    pub timeout_ms: u64,
    /// Interface de rede preferida (endereço IP). Validada antes do uso.
    pub preferred_iface: Option<String>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            udp_buffer_bytes: 4_194_304,
            timeout_ms: 5_000,
            preferred_iface: None,
        }
    }
}

/// Configurações do player.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct PlayerConfig {
    /// Tamanho do jitter buffer em milissegundos. Padrão: 100.
    pub jitter_buffer_ms: u64,
    /// Volume de reprodução (0.0–2.0). Padrão: 1.0.
    pub volume: f32,
    /// Fallback para renderização por CPU quando GPU indisponível. Padrão: false.
    pub fallback_cpu_render: bool,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            jitter_buffer_ms: 100,
            volume: 1.0,
            fallback_cpu_render: false,
        }
    }
}

/// Configurações do analisador de stream.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct AnalyzerConfig {
    /// Janela de medição de bitrate em segundos. Padrão: 1.
    pub bitrate_window_secs: u64,
    /// Histórico de bitrate mantido em segundos. Padrão: 60.
    pub bitrate_history_secs: u64,
    /// Limiar de jitter PCR em microssegundos. Padrão: 500.
    pub pcr_jitter_threshold_us: i64,
    /// Número de PIDs mais ativos exibidos. Padrão: 10.
    pub top_pids_count: usize,
    /// Capacidade máxima do log de erros. Padrão: 1 000.
    pub max_error_log_entries: usize,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            bitrate_window_secs: 1,
            bitrate_history_secs: 60,
            pcr_jitter_threshold_us: 500,
            top_pids_count: 10,
            max_error_log_entries: 1_000,
        }
    }
}

/// Configurações de interface gráfica.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct UiConfig {
    /// Tema escuro ativado. Padrão: true.
    pub dark_theme: bool,
    /// Largura inicial da janela em pixels. Padrão: 1400.
    pub window_width: u32,
    /// Altura inicial da janela em pixels. Padrão: 900.
    pub window_height: u32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            dark_theme: true,
            window_width: 1400,
            window_height: 900,
        }
    }
}

// ---------------------------------------------------------------------------
// AppConfig
// ---------------------------------------------------------------------------

/// SPEC-CFG-001
/// Configuração raiz da aplicação, carregada de `ironstream.toml`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
#[serde(default)]
pub struct AppConfig {
    pub network: NetworkConfig,
    pub player: PlayerConfig,
    pub analyzer: AnalyzerConfig,
    pub ui: UiConfig,
}

impl AppConfig {
    /// SPEC-CFG-001
    ///
    /// Carrega `ironstream.toml` na pasta do executável.
    ///
    /// Comportamento:
    /// - Arquivo ausente → `Default::default()` (sem log)
    /// - Arquivo inválido → log WARN + `Default::default()` (sem panic)
    /// - Arquivo válido (parcial ou completo) → campos ausentes usam `Default`
    ///
    /// Após a carga bem-sucedida dos defaults (arquivo ausente), gera o arquivo
    /// com os valores padrão para facilitar a edição pelo usuário.
    pub fn load_or_default() -> Self {
        let path = config_path();
        Self::load_from_path_or_default(&path)
    }

    /// Variante interna que aceita um `PathBuf` explícito (facilita testes).
    pub(crate) fn load_from_path_or_default(path: &PathBuf) -> Self {
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Arquivo ausente: gerar com valores padrão
                let cfg = AppConfig::default();
                cfg.write_default_to(path);
                cfg
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Falha ao ler ironstream.toml; usando configuração padrão"
                );
                AppConfig::default()
            }
            Ok(contents) => match toml::from_str::<AppConfig>(&contents) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "ironstream.toml inválido; usando configuração padrão"
                    );
                    AppConfig::default()
                }
            },
        }
    }

    /// Serializa a configuração padrão e grava em `path`.
    /// Erros de escrita são logados como WARN e ignorados (não fatal).
    fn write_default_to(&self, path: &PathBuf) {
        match toml::to_string_pretty(self) {
            Ok(contents) => {
                if let Err(e) = std::fs::write(path, contents) {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Não foi possível gravar ironstream.toml padrão"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Falha ao serializar configuração padrão");
            }
        }
    }
}

/// Retorna o caminho para `ironstream.toml` na pasta do executável.
/// Em caso de falha ao determinar o caminho, usa o diretório de trabalho atual.
fn config_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ironstream.toml")
}

// ---------------------------------------------------------------------------
// Testes
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // Helper: cria um arquivo temporário com conteúdo
    fn temp_toml(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ironstream.toml");
        let mut f = std::fs::File::create(&path).expect("create");
        f.write_all(content.as_bytes()).expect("write");
        (dir, path)
    }

    // Helper: retorna um caminho em diretório temporário SEM criar o arquivo
    fn temp_absent() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ironstream.toml");
        (dir, path)
    }

    /// SPEC-CFG-001 — arquivo ausente deve retornar exatamente AppConfig::default()
    #[test]
    fn spec_cfg_001_defaults_when_file_absent() {
        let (_dir, path) = temp_absent();
        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg, AppConfig::default());
    }

    /// SPEC-CFG-001 — arquivo ausente deve ser criado com valores padrão
    #[test]
    fn spec_cfg_001_creates_default_file_when_absent() {
        let (_dir, path) = temp_absent();
        assert!(!path.exists());
        let _cfg = AppConfig::load_from_path_or_default(&path);
        assert!(
            path.exists(),
            "ironstream.toml deve ser criado na primeira execução"
        );
    }

    /// SPEC-CFG-001 — arquivo criado na primeira execução deve ser válido TOML
    #[test]
    fn spec_cfg_001_generated_file_is_valid_toml() {
        let (_dir, path) = temp_absent();
        let _ = AppConfig::load_from_path_or_default(&path);
        let contents = std::fs::read_to_string(&path).expect("arquivo gerado");
        let parsed: AppConfig = toml::from_str(&contents).expect("TOML gerado deve ser válido");
        assert_eq!(parsed, AppConfig::default());
    }

    /// SPEC-CFG-001 — override parcial: campos ausentes usam Default
    #[test]
    fn spec_cfg_001_partial_override() {
        let toml_str = r#"
[network]
timeout_ms = 9999
"#;
        let (_dir, path) = temp_toml(toml_str);
        let cfg = AppConfig::load_from_path_or_default(&path);

        // Campo sobrescrito
        assert_eq!(cfg.network.timeout_ms, 9_999);
        // Campos ausentes usam Default
        assert_eq!(
            cfg.network.udp_buffer_bytes,
            NetworkConfig::default().udp_buffer_bytes
        );
        assert_eq!(cfg.player, PlayerConfig::default());
        assert_eq!(cfg.analyzer, AnalyzerConfig::default());
        assert_eq!(cfg.ui, UiConfig::default());
    }

    /// SPEC-CFG-001 — TOML inválido não deve causar panic; usa defaults
    #[test]
    fn spec_cfg_001_invalid_file_uses_defaults() {
        let (_dir, path) = temp_toml("[[[[invalid toml content");
        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg, AppConfig::default());
    }

    /// SPEC-CFG-001 — TOML com tipo errado num campo não deve causar panic
    #[test]
    fn spec_cfg_001_wrong_type_uses_defaults() {
        let toml_str = r#"
[network]
udp_buffer_bytes = "não é número"
"#;
        let (_dir, path) = temp_toml(toml_str);
        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg, AppConfig::default());
    }

    /// SPEC-CFG-001 — arquivo completo com todos os campos sobrescritos
    #[test]
    fn spec_cfg_001_full_override() {
        let toml_str = r#"
[network]
udp_buffer_bytes = 1048576
timeout_ms = 3000
preferred_iface = "192.168.1.1"

[player]
jitter_buffer_ms = 200
volume = 0.75
fallback_cpu_render = true

[analyzer]
bitrate_window_secs = 2
bitrate_history_secs = 30
pcr_jitter_threshold_us = 1000
top_pids_count = 5
max_error_log_entries = 500

[ui]
dark_theme = false
window_width = 1920
window_height = 1080
"#;
        let (_dir, path) = temp_toml(toml_str);
        let cfg = AppConfig::load_from_path_or_default(&path);

        assert_eq!(cfg.network.udp_buffer_bytes, 1_048_576);
        assert_eq!(cfg.network.timeout_ms, 3_000);
        assert_eq!(cfg.network.preferred_iface, Some("192.168.1.1".to_string()));
        assert!((cfg.player.volume - 0.75_f32).abs() < f32::EPSILON);
        assert!(cfg.player.fallback_cpu_render);
        assert_eq!(cfg.analyzer.top_pids_count, 5);
        assert!(!cfg.ui.dark_theme);
        assert_eq!(cfg.ui.window_width, 1920);
    }

    /// SPEC-CFG-001 — valores Default são os documentados na spec
    #[test]
    fn spec_cfg_001_default_values_match_spec() {
        let cfg = AppConfig::default();

        assert_eq!(cfg.network.udp_buffer_bytes, 4_194_304);
        assert_eq!(cfg.network.timeout_ms, 5_000);
        assert!(cfg.network.preferred_iface.is_none());

        assert_eq!(cfg.player.jitter_buffer_ms, 100);
        assert!((cfg.player.volume - 1.0_f32).abs() < f32::EPSILON);
        assert!(!cfg.player.fallback_cpu_render);

        assert_eq!(cfg.analyzer.bitrate_window_secs, 1);
        assert_eq!(cfg.analyzer.bitrate_history_secs, 60);
        assert_eq!(cfg.analyzer.pcr_jitter_threshold_us, 500);
        assert_eq!(cfg.analyzer.top_pids_count, 10);
        assert_eq!(cfg.analyzer.max_error_log_entries, 1_000);

        assert!(cfg.ui.dark_theme);
        assert_eq!(cfg.ui.window_width, 1400);
        assert_eq!(cfg.ui.window_height, 900);
    }
}
