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

/// Seleção de hardware acceleration para o decoder de vídeo.
///
/// SPEC-CFG-HW-001
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HwAccelChoice {
    /// Tenta D3D11VA quando o sistema suporta; cai em CPU caso contrário.
    #[default]
    Auto,
    /// Força D3D11VA; se não disponível, fica em CPU mas registra fallback.
    D3d11va,
    /// Desativa qualquer hwaccel; decode 100 % CPU.
    None,
}

impl HwAccelChoice {
    /// Faz parsing de uma string CLI/config (case-insensitive).
    ///
    /// SPEC-CFG-HW-001
    pub fn parse_cli(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "d3d11va" | "d3d11" | "dxva" | "dxva2" => Ok(Self::D3d11va),
            "none" | "off" | "cpu" | "sw" => Ok(Self::None),
            other => Err(format!(
                "valor inválido para --hwaccel: '{other}' (use auto|d3d11va|none)"
            )),
        }
    }

    /// Identificador estável para logs e telemetria.
    ///
    /// SPEC-CFG-HW-001
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::D3d11va => "d3d11va",
            Self::None => "none",
        }
    }
}

// Conversões com a variante UI (sem dependência de serde/TOML).
//
// SPEC-CFG-HW-001
impl From<ui_slint::HwAccelChoice> for HwAccelChoice {
    fn from(value: ui_slint::HwAccelChoice) -> Self {
        match value {
            ui_slint::HwAccelChoice::Auto => Self::Auto,
            ui_slint::HwAccelChoice::D3d11va => Self::D3d11va,
            ui_slint::HwAccelChoice::None => Self::None,
        }
    }
}

impl From<HwAccelChoice> for ui_slint::HwAccelChoice {
    fn from(value: HwAccelChoice) -> Self {
        match value {
            HwAccelChoice::Auto => Self::Auto,
            HwAccelChoice::D3d11va => Self::D3d11va,
            HwAccelChoice::None => Self::None,
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
    /// Seleciona automaticamente o primeiro serviço com streams A/V válidos ao
    /// receber a primeira PMT, sem sobrescrever uma seleção manual. Padrão: true.
    pub auto_play_first_service: bool,
    /// Aceleração de hardware do decoder de vídeo (D3D11VA).  Padrão: `Auto`.
    ///
    /// SPEC-CFG-HW-001
    pub hwaccel: HwAccelChoice,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            jitter_buffer_ms: 100,
            volume: 1.0,
            fallback_cpu_render: false,
            auto_play_first_service: true,
            hwaccel: HwAccelChoice::Auto,
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

/// Tipo de threading do decoder FFmpeg ([decoder] no ironstream.toml).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecoderThreadType {
    /// FFmpeg escolhe automaticamente a estratégia ideal.
    #[default]
    Auto,
    /// Frame threading — decodifica múltiplos frames em paralelo.
    Frame,
    /// Slice threading — decodifica fatias de um frame em paralelo.
    Slice,
}

/// Perfil de qualidade/velocidade do decodificador FFmpeg.
///
/// Sobrescreve `skip_loop_filter` e `flag2_fast` quando diferente de `default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecoderProfile {
    /// Configuração individual via `skip_loop_filter` e `flag2_fast` (padrão).
    #[default]
    Default,
    /// Perfil rápido: habilita `skip_loop_filter = NonRef` + `flag2_fast`.
    ///
    /// Reduz uso de CPU em ~20–30% em H.264 com leve impacto na qualidade de imagem.
    Fast,
    /// Perfil preciso: desabilita todos os atalhos de velocidade.
    ///
    /// Máxima qualidade de decodificação; ignora `skip_loop_filter` e `flag2_fast`.
    Accurate,
}

/// Configurações do decodificador FFmpeg (bloco `[decoder]` no ironstream.toml).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct DecoderConfig {
    /// Número de threads de decodificação. 0 = detectar automaticamente (num_cpus).
    pub thread_count: u32,
    /// Estratégia de threading.
    pub thread_type: DecoderThreadType,
    /// Habilita `skip_loop_filter=noref` (reduz CPU ~10–25 % em H.264).
    pub skip_loop_filter: bool,
    /// Habilita `CODEC_FLAG2_FAST` (desativa sub-ME, reduz CPU).
    pub flag2_fast: bool,
    /// Perfil de otimização (`fast` / `accurate` / `default`).
    ///
    /// `fast` sobrescreve `skip_loop_filter = true` e `flag2_fast = true`.
    /// `accurate` sobrescreve ambos para `false`.
    /// `default` usa os valores individuais acima.
    pub profile: DecoderProfile,
}

impl Default for DecoderConfig {
    fn default() -> Self {
        Self {
            thread_count: 0, // 0 = detectar em runtime via available_parallelism
            thread_type: DecoderThreadType::Auto,
            // skip_loop_filter habilitado por padrão: para uso de monitoramento
            // broadcast, pular o filtro de deblocking em frames não-referência
            // economiza ~15–25 % de CPU em HEVC com impacto mínimo na imagem.
            skip_loop_filter: true,
            flag2_fast: false,
            profile: DecoderProfile::Default,
        }
    }
}

/// Modo de operação persistido em `[ui] mode`.
///
/// Espelha [`ui_slint::AppMode`]; existe separado porque `ui-slint` não
/// depende de `serde`/`toml` e a direção de dependência é
/// `ui-slint → ts, av, net`, nunca o binário.
///
/// SPEC-PROBE-001
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AppModeChoice {
    /// Vídeo em tela cheia, painéis ocultos.
    Cinema,
    /// Player + painéis de análise.
    #[default]
    Broadcast,
    /// Monitoração contínua sem reprodução.
    Probe,
}

impl From<ui_slint::AppMode> for AppModeChoice {
    fn from(value: ui_slint::AppMode) -> Self {
        match value {
            ui_slint::AppMode::Cinema => Self::Cinema,
            ui_slint::AppMode::Broadcast => Self::Broadcast,
            ui_slint::AppMode::Probe => Self::Probe,
        }
    }
}

impl From<AppModeChoice> for ui_slint::AppMode {
    fn from(value: AppModeChoice) -> Self {
        match value {
            AppModeChoice::Cinema => Self::Cinema,
            AppModeChoice::Broadcast => Self::Broadcast,
            AppModeChoice::Probe => Self::Probe,
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
    /// Modo ativo na última execução, restaurado no start.
    ///
    /// SPEC-PROBE-001
    pub mode: AppModeChoice,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            dark_theme: true,
            window_width: 1400,
            window_height: 900,
            mode: AppModeChoice::Broadcast,
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
    pub decoder: DecoderConfig,
    /// Seção `[probe]` — SPEC-PROBE-015.
    ///
    /// Aditiva: um arquivo sem a seção mantém o comportamento atual de feed
    /// único vindo da barra de URL (§9).
    pub probe: probe::ProbeConfig,
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

    /// Persiste o modo ativo em `[ui] mode`.
    ///
    /// Edita **cirurgicamente** o arquivo em vez de reserializar o
    /// `AppConfig` inteiro: o `ironstream.toml` é editado à mão pelo operador
    /// (limiares de check, lista de feeds) e reescrevê-lo a cada clique no
    /// seletor apagaria comentários e ordem das seções.
    ///
    /// Falha de I/O é logada e ignorada — não perder o modo salvo é
    /// desejável, mas não ao ponto de derrubar a troca de modo.
    ///
    /// SPEC-PROBE-001
    pub fn persist_ui_mode(mode: AppModeChoice) {
        let path = config_path();
        Self::persist_ui_mode_at(&path, mode);
    }

    pub(crate) fn persist_ui_mode_at(path: &PathBuf, mode: AppModeChoice) {
        let label = match mode {
            AppModeChoice::Cinema => "cinema",
            AppModeChoice::Broadcast => "broadcast",
            AppModeChoice::Probe => "probe",
        };

        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut doc = match text.parse::<toml_edit::DocumentMut>() {
            Ok(doc) => doc,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "ironstream.toml inválido; modo não persistido"
                );
                return;
            }
        };

        doc["ui"]["mode"] = toml_edit::value(label);

        if let Err(e) = std::fs::write(path, doc.to_string()) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "não foi possível persistir [ui] mode"
            );
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

    /// SPEC-CFG-001 — defaults do bloco [decoder].
    ///
    /// `skip_loop_filter` é `true` por decisão explícita (ver o comentário em
    /// `DecoderConfig::default`): para monitoramento broadcast, pular o
    /// deblocking em frames não-referência economiza ~15–25 % de CPU em HEVC.
    /// A asserção anterior ainda cobrava o valor antigo e deixava a suíte
    /// vermelha.
    #[test]
    fn spec_cfg_001_decoder_defaults_conservative() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.decoder.thread_count, 0);
        assert_eq!(cfg.decoder.thread_type, DecoderThreadType::Auto);
        assert!(cfg.decoder.skip_loop_filter);
        assert!(!cfg.decoder.flag2_fast);
    }

    /// SPEC-CFG-001 — bloco [decoder] é desserializado corretamente do TOML
    #[test]
    fn spec_cfg_001_decoder_block_from_toml() {
        let toml_str = r#"
[decoder]
thread_count = 8
thread_type = "frame"
skip_loop_filter = true
flag2_fast = false
"#;
        let (_dir, path) = temp_toml(toml_str);
        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg.decoder.thread_count, 8);
        assert_eq!(cfg.decoder.thread_type, DecoderThreadType::Frame);
        assert!(cfg.decoder.skip_loop_filter);
        assert!(!cfg.decoder.flag2_fast);
    }

    /// SPEC-CFG-001 — bloco [decoder] ausente usa defaults conservadores
    #[test]
    fn spec_cfg_001_decoder_block_absent_uses_defaults() {
        let toml_str = r#"
[network]
timeout_ms = 5000
"#;
        let (_dir, path) = temp_toml(toml_str);
        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg.decoder, DecoderConfig::default());
    }

    // ── AppMode / [probe] (SPEC-PROBE-001 · SPEC-PROBE-015) ────────────────

    /// SPEC-PROBE-001 — o modo default é Broadcast (sem regressão de player).
    #[test]
    fn spec_probe_001_default_mode_is_broadcast() {
        assert_eq!(AppConfig::default().ui.mode, AppModeChoice::Broadcast);
    }

    /// SPEC-PROBE-001 — `[ui] mode` é lido do TOML e convertido para a UI.
    #[test]
    fn spec_probe_001_mode_is_read_from_toml() {
        let (_dir, path) = temp_toml("[ui]\nmode = \"probe\"\n");
        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg.ui.mode, AppModeChoice::Probe);
        assert_eq!(
            ui_slint::AppMode::from(cfg.ui.mode),
            ui_slint::AppMode::Probe
        );
    }

    /// SPEC-PROBE-001 — o modo persiste e é restaurado na execução seguinte.
    #[test]
    fn spec_probe_001_mode_persists_and_is_restored() {
        let (_dir, path) = temp_absent();
        let _ = AppConfig::load_from_path_or_default(&path);

        AppConfig::persist_ui_mode_at(&path, AppModeChoice::Probe);
        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg.ui.mode, AppModeChoice::Probe);

        AppConfig::persist_ui_mode_at(&path, AppModeChoice::Cinema);
        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg.ui.mode, AppModeChoice::Cinema);
    }

    /// SPEC-PROBE-001 — persistir o modo não destrói comentários nem outras
    /// seções editadas à mão pelo operador.
    #[test]
    fn spec_probe_001_persisting_mode_preserves_handwritten_toml() {
        let toml_str = r#"
# limiares combinados com a operação
[network]
timeout_ms = 7000  # comentário do operador

[[probe.feeds]]
name = "0084_CANAL_A"
url  = "rtp://@239.15.0.183:50000"
"#;
        let (_dir, path) = temp_toml(toml_str);
        AppConfig::persist_ui_mode_at(&path, AppModeChoice::Probe);

        let text = std::fs::read_to_string(&path).expect("relê");
        assert!(text.contains("# limiares combinados com a operação"));
        assert!(text.contains("# comentário do operador"));
        assert!(text.contains("0084_CANAL_A"));
        assert!(text.contains("mode = \"probe\""));

        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg.ui.mode, AppModeChoice::Probe);
        assert_eq!(cfg.network.timeout_ms, 7_000);
        assert_eq!(cfg.probe.feeds.len(), 1);
    }

    /// SPEC-PROBE-015 — a seção `[probe]` é aditiva: ausente ⇒ defaults, e um
    /// arquivo sem ela mantém o comportamento de feed único.
    #[test]
    fn spec_probe_015_probe_section_is_additive() {
        let (_dir, path) = temp_toml("[network]\ntimeout_ms = 5000\n");
        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg.probe, probe::ProbeConfig::default());
        assert!(cfg.probe.effective_feeds().is_empty());
    }

    /// SPEC-PROBE-015 — `[[probe.feeds]]` e `[probe.checks.*]` desserializam.
    #[test]
    fn spec_probe_015_feeds_and_check_overrides_parse() {
        let toml_str = r#"
[probe]
profile_version = 3
snapshot_interval_secs = 7

[[probe.feeds]]
name = "0084_CANAL_A"
url  = "rtp://@239.15.0.183:50000"
fec  = "auto"

[[probe.feeds]]
name = "0116_CANAL_B"
url  = "udp://@239.15.0.190:50000"
fec  = "off"

[probe.checks.cc_error]
threshold = 5.0
"#;
        let (_dir, path) = temp_toml(toml_str);
        let cfg = AppConfig::load_from_path_or_default(&path);

        assert_eq!(cfg.probe.profile_version, 3);
        assert_eq!(cfg.probe.snapshot_interval_secs, 7);
        let feeds = cfg.probe.effective_feeds();
        assert_eq!(feeds.len(), 2);
        assert_eq!(feeds[0].name, "0084_CANAL_A");
        assert_eq!(feeds[1].fec, probe::FecMode::Off);
        assert_eq!(
            cfg.probe.checks.get("cc_error").and_then(|o| o.threshold),
            Some(5.0)
        );
    }

    /// SPEC-PROBE-015 — arquivo ausente gera a seção `[probe]` com defaults.
    #[test]
    fn spec_probe_015_absent_file_generates_probe_section() {
        let (_dir, path) = temp_absent();
        let _ = AppConfig::load_from_path_or_default(&path);
        let text = std::fs::read_to_string(&path).expect("arquivo gerado");
        assert!(text.contains("[probe]"), "faltou a seção [probe]:\n{text}");
        assert!(text.contains("profile_version"));
        let back: AppConfig = toml::from_str(&text).expect("TOML gerado deve ser válido");
        assert_eq!(back, AppConfig::default());
    }

    // ── HwAccelChoice (SPEC-CFG-HW-001) ────────────────────────────────────

    /// SPEC-CFG-HW-001 — default de hwaccel é Auto.
    #[test]
    fn spec_cfg_hw_001_default_is_auto() {
        let cfg = AppConfig::default();
        assert_eq!(cfg.player.hwaccel, HwAccelChoice::Auto);
    }

    /// SPEC-CFG-HW-001 — `--hwaccel` CLI aceita auto/d3d11va/none.
    #[test]
    fn spec_cfg_hw_001_parse_cli_canonical_forms() {
        assert_eq!(HwAccelChoice::parse_cli("auto"), Ok(HwAccelChoice::Auto));
        assert_eq!(
            HwAccelChoice::parse_cli("d3d11va"),
            Ok(HwAccelChoice::D3d11va)
        );
        assert_eq!(HwAccelChoice::parse_cli("none"), Ok(HwAccelChoice::None));
    }

    /// SPEC-CFG-HW-001 — parse_cli é case-insensitive e aceita aliases.
    #[test]
    fn spec_cfg_hw_001_parse_cli_aliases() {
        assert_eq!(HwAccelChoice::parse_cli("AUTO"), Ok(HwAccelChoice::Auto));
        assert_eq!(
            HwAccelChoice::parse_cli("D3D11"),
            Ok(HwAccelChoice::D3d11va)
        );
        assert_eq!(HwAccelChoice::parse_cli("Off"), Ok(HwAccelChoice::None));
        assert_eq!(HwAccelChoice::parse_cli("cpu"), Ok(HwAccelChoice::None));
    }

    /// SPEC-CFG-HW-001 — valores inválidos retornam Err com contexto.
    #[test]
    fn spec_cfg_hw_001_parse_cli_rejects_invalid() {
        let err = HwAccelChoice::parse_cli("nvdec").unwrap_err();
        assert!(err.contains("nvdec"));
        assert!(err.contains("auto"));
    }

    /// SPEC-CFG-HW-001 — `[player].hwaccel` é desserializado do TOML.
    #[test]
    fn spec_cfg_hw_001_player_hwaccel_from_toml() {
        let toml_str = r#"
[player]
hwaccel = "d3d11va"
"#;
        let (_dir, path) = temp_toml(toml_str);
        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg.player.hwaccel, HwAccelChoice::D3d11va);
    }

    /// SPEC-CFG-HW-001 — `hwaccel = "none"` desativa hwaccel via config.
    #[test]
    fn spec_cfg_hw_001_player_hwaccel_none_from_toml() {
        let toml_str = r#"
[player]
hwaccel = "none"
"#;
        let (_dir, path) = temp_toml(toml_str);
        let cfg = AppConfig::load_from_path_or_default(&path);
        assert_eq!(cfg.player.hwaccel, HwAccelChoice::None);
    }
}
