//! Retenção de sessões antigas em disco.
//!
//! SPEC-PROBE-016 · RNF-PRB-005

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Uma pasta de run candidata à remoção.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEntry {
    pub dir: PathBuf,
    pub modified: SystemTime,
    pub bytes: u64,
}

/// Resultado de uma passada de retenção.
///
/// SPEC-PROBE-016 — "remoção é logada"; este relatório é o que vai ao log e
/// o que os testes inspecionam.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetentionReport {
    pub removed: Vec<PathBuf>,
    pub removed_by_age: usize,
    pub removed_by_size: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub failures: usize,
}

/// Lista os runs sob `root`, do mais antigo ao mais recente.
///
/// Entradas ilegíveis são ignoradas em vez de abortar a varredura
/// (RNF-PRB-003).
pub fn list_runs(root: &Path) -> Vec<RunEntry> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };

    let mut runs: Vec<RunEntry> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let dir = e.path();
            let modified = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            RunEntry {
                bytes: dir_size(&dir),
                dir,
                modified,
            }
        })
        .collect();

    runs.sort_by_key(|r| r.modified);
    runs
}

/// Soma recursiva do tamanho de uma pasta.
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Aplica a política de retenção.
///
/// Remove por idade (`retention_days`) e, se ainda exceder, por tamanho total
/// (`max_disk_mb`), sempre do mais antigo para o mais recente.  `keep` é a
/// pasta do run em curso e **nunca** é removida.
///
/// SPEC-PROBE-016
pub fn apply(
    root: &Path,
    retention_days: u64,
    max_disk_mb: u64,
    keep: Option<&Path>,
    now: SystemTime,
) -> RetentionReport {
    let mut report = RetentionReport::default();
    let runs = list_runs(root);
    report.bytes_before = runs.iter().map(|r| r.bytes).sum();
    report.bytes_after = report.bytes_before;

    let max_age = Duration::from_secs(retention_days.saturating_mul(86_400));
    let budget = max_disk_mb.saturating_mul(1024 * 1024);

    let protected = |dir: &Path| keep.is_some_and(|k| same_path(k, dir));

    // 1. Por idade.
    let mut survivors: Vec<RunEntry> = Vec::with_capacity(runs.len());
    for run in runs {
        let expired = retention_days > 0
            && now
                .duration_since(run.modified)
                .is_ok_and(|age| age > max_age);

        if expired && !protected(&run.dir) {
            if remove(&run.dir, "idade", &mut report) {
                report.removed_by_age += 1;
            }
            continue;
        }
        survivors.push(run);
    }

    // 2. Por tamanho total, do mais antigo em diante.
    if budget > 0 {
        let mut idx = 0usize;
        while report.bytes_after > budget && idx < survivors.len() {
            let run = &survivors[idx];
            idx += 1;
            if protected(&run.dir) {
                continue;
            }
            if remove(&run.dir, "tamanho", &mut report) {
                report.removed_by_size += 1;
            }
        }
    }

    report
}

fn remove(dir: &Path, reason: &str, report: &mut RetentionReport) -> bool {
    let bytes = dir_size(dir);
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {
            tracing::info!(
                path = %dir.display(),
                reason,
                mb = bytes / (1024 * 1024),
                "probe: sessão removida por retenção"
            );
            report.bytes_after = report.bytes_after.saturating_sub(bytes);
            report.removed.push(dir.to_path_buf());
            true
        }
        Err(e) => {
            tracing::warn!(path = %dir.display(), error = %e, "probe: falha ao remover sessão");
            report.failures += 1;
            false
        }
    }
}

/// Compara caminhos tolerando `.`/`..` e diferenças de canonicalização.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_run(root: &Path, name: &str, bytes: usize, age: Duration) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("feed-0")).expect("cria run");
        std::fs::write(dir.join("feed-0").join("metrics.csv"), vec![b'x'; bytes])
            .expect("grava csv");
        // `filetime` não é dependência: o teste controla o "agora" em vez do
        // mtime, o que é suficiente para exercitar a política.
        let _ = age;
        dir
    }

    /// SPEC-PROBE-016 — sessões mais velhas que `retention_days` são removidas.
    #[test]
    fn spec_probe_016_removes_runs_older_than_retention() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let old = make_run(tmp.path(), "2020-01-01T00-00-00_run", 64, Duration::ZERO);

        // "Agora" 30 dias no futuro em relação ao mtime real dos arquivos.
        let now = SystemTime::now() + Duration::from_secs(30 * 86_400);
        let report = apply(tmp.path(), 14, 4096, None, now);

        assert_eq!(report.removed_by_age, 1);
        assert!(!old.exists());
    }

    /// SPEC-PROBE-016 — a sessão em curso nunca é apagada.
    #[test]
    fn spec_probe_016_never_removes_the_running_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let current = make_run(tmp.path(), "2026-08-07T09-15-32_run", 64, Duration::ZERO);
        let other = make_run(tmp.path(), "2026-08-06T09-15-32_run", 64, Duration::ZERO);

        let now = SystemTime::now() + Duration::from_secs(365 * 86_400);
        let report = apply(tmp.path(), 1, 4096, Some(&current), now);

        assert!(current.exists(), "o run em curso não pode ser removido");
        assert!(!other.exists());
        assert_eq!(report.removed.len(), 1);
    }

    /// SPEC-PROBE-016 — excedido o orçamento de disco, remove do mais antigo
    /// até caber.
    #[test]
    fn spec_probe_016_trims_by_total_size_oldest_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // 3 runs de ~400 KiB; orçamento de 1 MiB deve sobrar 2.
        for i in 0..3 {
            make_run(
                tmp.path(),
                &format!("2026-08-0{}T00-00-00_run", i + 1),
                400 * 1024,
                Duration::ZERO,
            );
            // Garante ordem de mtime distinta entre os runs.
            std::thread::sleep(Duration::from_millis(15));
        }

        let before = list_runs(tmp.path());
        assert_eq!(before.len(), 3);
        let oldest = before[0].dir.clone();

        let report = apply(tmp.path(), 0, 1, None, SystemTime::now());

        assert!(report.removed_by_size >= 1);
        assert!(!oldest.exists(), "o mais antigo deve sair primeiro");
        assert!(report.bytes_after <= 1024 * 1024);
    }

    /// SPEC-PROBE-016 — dentro dos limites, nada é removido.
    #[test]
    fn spec_probe_016_keeps_everything_within_budget() {
        let tmp = tempfile::tempdir().expect("tempdir");
        make_run(tmp.path(), "2026-08-07T00-00-00_run", 1024, Duration::ZERO);
        let report = apply(tmp.path(), 14, 4096, None, SystemTime::now());
        assert!(report.removed.is_empty());
        assert_eq!(report.failures, 0);
    }

    /// RNF-PRB-003 — raiz inexistente não faz panic.
    #[test]
    fn rnf_prb_003_missing_root_is_a_noop() {
        let report = apply(
            Path::new("./nao-existe-probe-sessions"),
            14,
            4096,
            None,
            SystemTime::now(),
        );
        assert_eq!(report, RetentionReport::default());
    }
}
