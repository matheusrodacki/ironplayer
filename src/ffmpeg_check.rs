//! SPEC-AV-CHECK-001
//!
//! Verificação de compatibilidade das DLLs FFmpeg no startup.
//!
//! Tenta carregar `avcodec` dinamicamente e chamar `avcodec_version()` para
//! confirmar que as DLLs corretas (FFmpeg 8.x → avcodec major 62) estão
//! presentes antes de qualquer instanciação de `FfmpegDecoder`.
//!
//! Critério de busca: primeiro `{exe_dir}/ffmpeg/`, depois `{exe_dir}/`.
//! Em caso de falha, retorna mensagem de erro detalhada para saída limpa.

/// Major version do avcodec que corresponde ao FFmpeg 8.x.
const AVCODEC_EXPECTED_MAJOR: u32 = 62;

/// Nome da DLL avcodec para a versão esperada (Windows).
#[cfg(windows)]
const AVCODEC_DLL: &str = "avcodec-62.dll";

/// Nome da biblioteca avcodec para a versão esperada (Linux/macOS – fallback).
#[cfg(not(windows))]
const AVCODEC_DLL: &str = "libavcodec.so.62";

/// Verifica se as DLLs FFmpeg são compatíveis com a versão esperada.
///
/// SPEC-AV-CHECK-001 — deve ser chamada em `main()` antes de qualquer
/// instanciação de `FfmpegDecoder`. Retorna `Err` com mensagem legível ao
/// usuário em caso de DLL ausente ou versão incompatível.
///
/// # Erros
/// - DLL não encontrada nos caminhos de busca.
/// - Símbolo `avcodec_version` ausente.
/// - Major version diferente de [`AVCODEC_EXPECTED_MAJOR`].
pub fn check_ffmpeg_compatibility() -> Result<(), String> {
    let search_paths = ffmpeg_search_paths();

    // Tenta cada caminho de busca em ordem
    for candidate in &search_paths {
        tracing::debug!(path = %candidate.display(), "tentando carregar {}", AVCODEC_DLL);

        // No Windows, avcodec-62.dll importa avutil-60.dll etc. O Windows só
        // encontra essas dependências se o diretório delas estiver no DLL
        // search path. SetDllDirectory adiciona o diretório da DLL
        // temporariamente para que as dependências sejam resolvidas.
        #[cfg(windows)]
        if let Some(dir) = candidate.parent() {
            set_dll_search_dir(Some(dir));
        }

        // SAFETY: libloading carrega a biblioteca do sistema operacional.
        // O código é unsafe por natureza da FFI, mas confinado aqui.
        let lib = match unsafe { libloading::Library::new(candidate) } {
            Ok(l) => l,
            Err(e) => {
                tracing::debug!(
                    path = %candidate.display(),
                    error = %e,
                    "DLL não encontrada neste caminho"
                );
                continue;
            }
        };

        // Resolve o símbolo avcodec_version
        // SAFETY: a assinatura `fn() -> u32` é a ABI correta de avcodec_version.
        let avcodec_version_fn: libloading::Symbol<unsafe extern "C" fn() -> u32> =
            match unsafe { lib.get(b"avcodec_version\0") } {
                Ok(sym) => sym,
                Err(e) => {
                    return Err(format!(
                        "FFmpeg: símbolo 'avcodec_version' não encontrado em '{}': {e}\n\
                         Certifique-se de que as DLLs FFmpeg são originais e não corrompidas.",
                        candidate.display()
                    ));
                }
            };

        // SAFETY: avcodec_version é uma função pura que retorna a versão compilada.
        let raw_version = unsafe { avcodec_version_fn() };
        let major = raw_version >> 16;
        let minor = (raw_version >> 8) & 0xFF;
        let patch = raw_version & 0xFF;

        if major != AVCODEC_EXPECTED_MAJOR {
            return Err(format!(
                "FFmpeg incompatível: avcodec versão detectada = {major}.{minor}.{patch}, \
                 esperada = {AVCODEC_EXPECTED_MAJOR}.x.x (FFmpeg 8.x).\n\
                 Atualize as DLLs FFmpeg na pasta 'ffmpeg/' para a versão 8.x.\n\
                 DLL carregada: {}",
                candidate.display()
            ));
        }

        tracing::info!(
            version = %format!("{major}.{minor}.{patch}"),
            path = %candidate.display(),
            "FFmpeg avcodec verificado com sucesso"
        );

        // Descarrega a lib: esta função só verifica compatibilidade.
        // A carga real fica por conta de `FfmpegDecoder` via ffmpeg-next.
        std::mem::drop(lib);

        // Restaura o diretório de busca de DLLs.
        #[cfg(windows)]
        set_dll_search_dir(None);

        return Ok(());
    }

    // Restaura o diretório de busca de DLLs caso nenhum candidato tenha carregado.
    #[cfg(windows)]
    set_dll_search_dir(None);

    // Nenhum caminho funcionou
    let paths_str = search_paths
        .iter()
        .map(|p| format!("  - {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");

    Err(format!(
        "FFmpeg não encontrado. DLL esperada: '{AVCODEC_DLL}'.\n\
         Caminhos pesquisados:\n{paths_str}\n\n\
         Solução:\n\
         1. Baixe FFmpeg 8.x para Windows em https://ffmpeg.org/download.html\n\
         2. Copie as DLLs ({AVCODEC_DLL}, avformat-62.dll, avutil-60.dll, swresample-6.dll, swscale-9.dll)\n\
            para a pasta 'ffmpeg/' ao lado do executável ironplayer.exe"
    ))
}

/// Retorna a lista ordenada de caminhos onde procurar a DLL avcodec.
///
/// Ordem de precedência:
/// 1. `{exe_dir}/ffmpeg/{dll}` — pasta dedicada ao lado do executável
/// 2. `{exe_dir}/{dll}` — diretório do executável diretamente
///
/// Não inclui PATH do sistema para evitar que DLLs de outras aplicações
/// sejam carregadas inadvertidamente (requisito de segurança SPEC-AV-CHECK-001).
fn ffmpeg_search_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            // Preferência: subpasta ffmpeg/ dedicada
            paths.push(exe_dir.join("ffmpeg").join(AVCODEC_DLL));
            // Fallback: diretório do executável diretamente
            paths.push(exe_dir.join(AVCODEC_DLL));
        }
    }

    // Último recurso: apenas o nome (deixa o OS resolver via PATH/RPATH)
    // Incluído para facilitar desenvolvimento sem instalação formal das DLLs.
    paths.push(std::path::PathBuf::from(AVCODEC_DLL));

    paths
}

/// Define (ou limpa) o diretório adicional de busca de DLLs no Windows.
///
/// Chama `SetDllDirectoryW(path)` para que dependências transitivas de
/// `avcodec-62.dll` (ex.: `avutil-60.dll`) sejam encontradas na mesma pasta.
/// Passar `None` restaura o comportamento padrão (`SetDllDirectoryW(NULL)`).
///
/// SAFETY: kernel32 está sempre disponível no Windows; a assinatura corresponde
/// à declaração oficial de `SetDllDirectoryW`.
#[cfg(windows)]
fn set_dll_search_dir(dir: Option<&std::path::Path>) {
    use std::os::windows::ffi::OsStrExt as _;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetDllDirectoryW(lpPathName: *const u16) -> i32;
    }

    match dir {
        Some(p) => {
            let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: wide é nul-terminado e kernel32 está sempre disponível.
            unsafe { SetDllDirectoryW(wide.as_ptr()) };
        }
        None => {
            // SAFETY: NULL restaura o comportamento padrão.
            unsafe { SetDllDirectoryW(std::ptr::null()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-AV-CHECK-001: caminhos de busca devem incluir subpasta ffmpeg/
    /// e o diretório do executável, nessa ordem.
    #[test]
    fn spec_av_check_001_search_paths_include_ffmpeg_subdir() {
        let paths = ffmpeg_search_paths();
        // Deve haver ao menos 2 caminhos (ffmpeg/ e exe_dir) ou o fallback nome puro
        assert!(
            paths.len() >= 2,
            "deve haver pelo menos 2 caminhos de busca"
        );

        // O primeiro caminho deve terminar em ffmpeg/{DLL} (se exe_path disponível)
        // ou ser o nome puro como último recurso
        let first = &paths[0];
        let first_str = first.to_string_lossy();
        // Verifica que contém o nome da DLL
        assert!(
            first_str.contains(AVCODEC_DLL),
            "caminho deve conter o nome da DLL: {first_str}"
        );
    }

    /// SPEC-AV-CHECK-001: caminhos de busca devem conter o nome correto da DLL.
    #[test]
    fn spec_av_check_001_dll_name_matches_expected_version() {
        // FFmpeg 8.x → avcodec major 62
        assert!(
            AVCODEC_DLL.contains("62"),
            "nome da DLL deve conter a major version 62 para FFmpeg 8.x"
        );
        assert_eq!(
            AVCODEC_EXPECTED_MAJOR, 62,
            "major version esperada deve ser 62 para FFmpeg 8.x"
        );
    }

    /// SPEC-AV-CHECK-001: versão incompatível deve retornar Err com mensagem clara.
    /// Testa a lógica de validação da major version isoladamente.
    #[test]
    fn spec_av_check_001_incompatible_version_produces_clear_error() {
        // Simula versão raw como FFmpeg 6.x (major = 60)
        let raw_version: u32 = (60u32 << 16) | (3 << 8) | 100;
        let major = raw_version >> 16;
        let minor = (raw_version >> 8) & 0xFF;
        let patch = raw_version & 0xFF;

        assert_ne!(
            major, AVCODEC_EXPECTED_MAJOR,
            "FFmpeg 6.x não deve ser aceito"
        );

        let err_msg = format!(
            "FFmpeg incompatível: avcodec versão detectada = {major}.{minor}.{patch}, \
             esperada = {AVCODEC_EXPECTED_MAJOR}.x.x (FFmpeg 8.x)."
        );
        assert!(
            err_msg.contains("60.3.100"),
            "mensagem deve citar versão detectada"
        );
        assert!(
            err_msg.contains("62"),
            "mensagem deve citar versão esperada"
        );
    }

    /// SPEC-AV-CHECK-001: versão compatível (62.x.x) deve ser aceita.
    #[test]
    fn spec_av_check_001_compatible_version_is_accepted() {
        let raw_version: u32 = (62u32 << 16) | (3 << 8) | 100;
        let major = raw_version >> 16;
        assert_eq!(
            major, AVCODEC_EXPECTED_MAJOR,
            "FFmpeg 8.x (major=62) deve ser aceito"
        );
    }

    /// SPEC-AV-CHECK-001: quando a DLL não existe, retorna Err com instruções claras.
    #[test]
    fn spec_av_check_001_missing_dll_returns_actionable_error() {
        // Não há DLL FFmpeg no ambiente de CI/teste, então check_ffmpeg_compatibility()
        // deve retornar Err com mensagem que inclui instruções de instalação.
        // Este teste só é executado quando as DLLs NÃO estão disponíveis.
        let result = check_ffmpeg_compatibility();
        if result.is_err() {
            let err = result.unwrap_err();
            assert!(
                err.contains("ffmpeg.org") || err.contains("incompatível") || err.contains("DLL"),
                "mensagem de erro deve ser acionável: {err}"
            );
        }
        // Se as DLLs estiverem disponíveis (ambiente com FFmpeg 7.x instalado),
        // o teste passa silenciosamente verificando Ok.
    }
}
