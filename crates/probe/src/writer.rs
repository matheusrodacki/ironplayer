//! Writer global: drena a fila de todos os feeds e persiste em disco.
//!
//! §5.4 — o writer é **global de propósito**: um writer por feed dobraria o
//! I/O concorrente num disco de notebook.  O `probe-engine` nunca faz I/O no
//! próprio tick; escrita lenta (disco de notebook, antivírus) não pode
//! atrasar a amostragem.
//!
//! SPEC-PROBE-006 · RNF-PRB-004 · RNF-PRB-005

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};

/// Capacidade da fila de escrita.
///
/// RNF-PRB-004 — bounded; descarte é contabilizado, nunca silencioso.
/// 4096 linhas cobrem ~34 min de amostras de 2 feeds a 1 Hz, folga muito
/// acima de qualquer stall plausível de disco.
pub const CAP_WRITE_QUEUE: usize = 4096;

/// Uma unidade de trabalho do writer.
///
/// SPEC-PROBE-006
#[derive(Debug, Clone)]
pub enum WriteJob {
    /// Cria/trunca o arquivo e escreve a primeira linha (cabeçalho do CSV).
    Create { path: PathBuf, header: String },
    /// Anexa uma linha (a quebra de linha é adicionada pelo writer).
    Append { path: PathBuf, line: String },
    /// Fecha e libera o arquivo — chamado ao encerrar a sessão do feed.
    Close { path: PathBuf },
    /// Força flush imediato de tudo (encerramento do run).
    FlushNow,
}

/// Contadores de saúde do writer, compartilhados com o autodiagnóstico.
///
/// SPEC-PROBE-013 · RNF-PRB-004
#[derive(Debug, Default)]
pub struct WriterStats {
    /// Linhas descartadas por fila cheia.
    pub dropped: AtomicU64,
    /// Linhas efetivamente escritas.
    pub written: AtomicU64,
    /// Erros de I/O acumulados.
    pub io_errors: AtomicU64,
}

/// Ponta de envio do writer, clonável por feed.
///
/// SPEC-PROBE-006
#[derive(Debug, Clone)]
pub struct WriterHandle {
    tx: Sender<WriteJob>,
    stats: Arc<WriterStats>,
}

impl WriterHandle {
    /// Enfileira um job sem bloquear.
    ///
    /// Devolve `false` quando a fila está cheia — o descarte é contabilizado
    /// em [`WriterStats::dropped`] e vira medição do check de descarte local
    /// (SPEC-PROBE-013), nunca um descarte silencioso (RNF-PRB-004).
    pub fn send(&self, job: WriteJob) -> bool {
        match self.tx.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                let n = self.stats.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n.is_multiple_of(100) {
                    tracing::warn!(dropped = n, "probe-writer: fila cheia — linha descartada");
                }
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Anexa uma linha ao arquivo indicado.
    pub fn append(&self, path: PathBuf, line: String) -> bool {
        self.send(WriteJob::Append { path, line })
    }

    /// Estatísticas compartilhadas.
    ///
    /// SPEC-PROBE-013
    pub fn stats(&self) -> Arc<WriterStats> {
        Arc::clone(&self.stats)
    }

    /// Total de linhas descartadas até agora.
    pub fn dropped(&self) -> u64 {
        self.stats.dropped.load(Ordering::Relaxed)
    }
}

/// Estado do writer, executado na thread `probe-writer`.
///
/// §5.4
pub struct ProbeWriter {
    rx: Receiver<WriteJob>,
    stats: Arc<WriterStats>,
    flush_interval: Duration,
    files: HashMap<PathBuf, BufWriter<File>>,
}

/// Cria a fila e o writer.
///
/// SPEC-PROBE-006
pub fn writer_channel(flush_interval: Duration) -> (WriterHandle, ProbeWriter) {
    let (tx, rx) = bounded(CAP_WRITE_QUEUE);
    let stats = Arc::new(WriterStats::default());
    (
        WriterHandle {
            tx,
            stats: Arc::clone(&stats),
        },
        ProbeWriter {
            rx,
            stats,
            flush_interval: flush_interval.max(Duration::from_millis(250)),
            files: HashMap::new(),
        },
    )
}

impl ProbeWriter {
    /// Roda até a fila ser desconectada (todos os `WriterHandle` dropados).
    ///
    /// O flush periódico é o que limita a perda a `flush_interval_secs` de
    /// amostras num `taskkill /f` — o arquivo continua legível porque só
    /// linhas inteiras são escritas.
    ///
    /// SPEC-PROBE-006
    pub fn run(mut self) {
        crate::power::lower_current_thread_priority();
        tracing::info!(
            flush_ms = self.flush_interval.as_millis() as u64,
            "probe-writer: iniciado"
        );

        loop {
            match self.rx.recv_timeout(self.flush_interval) {
                Ok(job) => {
                    self.handle(job);
                    // Drena o que já chegou antes de voltar a esperar: evita
                    // um flush por linha quando dois feeds ticam juntos.
                    while let Ok(next) = self.rx.try_recv() {
                        self.handle(next);
                    }
                }
                Err(RecvTimeoutError::Timeout) => self.flush_all(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        self.flush_all();
        self.files.clear();
        tracing::info!(
            written = self.stats.written.load(Ordering::Relaxed),
            dropped = self.stats.dropped.load(Ordering::Relaxed),
            io_errors = self.stats.io_errors.load(Ordering::Relaxed),
            "probe-writer: encerrado"
        );
    }

    fn handle(&mut self, job: WriteJob) {
        match job {
            WriteJob::Create { path, header } => {
                if let Some(parent) = path.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        self.io_error(&path, &e);
                        return;
                    }
                }
                match OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&path)
                {
                    Ok(file) => {
                        let mut w = BufWriter::new(file);
                        if !header.is_empty() {
                            if let Err(e) = writeln!(w, "{header}") {
                                self.io_error(&path, &e);
                            }
                        }
                        // Cabeçalho vai ao disco na hora: um `taskkill` no
                        // primeiro segundo ainda deixa um CSV interpretável.
                        if let Err(e) = w.flush() {
                            self.io_error(&path, &e);
                        }
                        self.files.insert(path, w);
                    }
                    Err(e) => self.io_error(&path, &e),
                }
            }
            WriteJob::Append { path, line } => {
                if !self.files.contains_key(&path) {
                    // Arquivo ainda não aberto (ex.: `Create` descartado por
                    // fila cheia): abre em modo append para não perder a linha.
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match OpenOptions::new().create(true).append(true).open(&path) {
                        Ok(file) => {
                            self.files.insert(path.clone(), BufWriter::new(file));
                        }
                        Err(e) => {
                            self.io_error(&path, &e);
                            return;
                        }
                    }
                }
                if let Some(w) = self.files.get_mut(&path) {
                    match writeln!(w, "{line}") {
                        Ok(()) => {
                            self.stats.written.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(e) => self.io_error(&path, &e),
                    }
                }
            }
            WriteJob::Close { path } => {
                if let Some(mut w) = self.files.remove(&path) {
                    if let Err(e) = w.flush() {
                        self.io_error(&path, &e);
                    }
                }
            }
            WriteJob::FlushNow => self.flush_all(),
        }
    }

    fn flush_all(&mut self) {
        let mut failed: Vec<PathBuf> = Vec::new();
        for (path, w) in self.files.iter_mut() {
            if let Err(e) = w.flush() {
                tracing::warn!(path = %path.display(), error = %e, "probe-writer: flush falhou");
                failed.push(path.clone());
            }
        }
        if !failed.is_empty() {
            self.stats
                .io_errors
                .fetch_add(failed.len() as u64, Ordering::Relaxed);
        }
    }

    fn io_error(&self, path: &std::path::Path, e: &std::io::Error) {
        self.stats.io_errors.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(path = %path.display(), error = %e, "probe-writer: erro de I/O");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC-PROBE-006 — o CSV nasce com cabeçalho e recebe linhas anexadas.
    #[test]
    fn spec_probe_006_creates_file_with_header_and_appends() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("feed-0").join("metrics.csv");

        let (handle, writer) = writer_channel(Duration::from_millis(250));
        let t = std::thread::spawn(move || writer.run());

        assert!(handle.send(WriteJob::Create {
            path: path.clone(),
            header: "a,b,c".into(),
        }));
        for i in 0..3 {
            assert!(handle.append(path.clone(), format!("{i},{i},{i}")));
        }
        drop(handle);
        t.join().expect("writer encerra");

        let text = std::fs::read_to_string(&path).expect("lê CSV");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines, vec!["a,b,c", "0,0,0", "1,1,1", "2,2,2"]);
    }

    /// SPEC-PROBE-006 — o flush periódico deixa o arquivo legível sem que o
    /// writer tenha encerrado (é o que limita a perda num `taskkill /f`).
    #[test]
    fn spec_probe_006_periodic_flush_keeps_file_readable_while_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");

        let (handle, writer) = writer_channel(Duration::from_millis(50));
        let t = std::thread::spawn(move || writer.run());

        handle.send(WriteJob::Create {
            path: path.clone(),
            header: String::new(),
        });
        handle.append(path.clone(), "{\"a\":1}".into());

        // Espera dois ciclos de flush sem encerrar o writer.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut content = String::new();
        while std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(60));
            content = std::fs::read_to_string(&path).unwrap_or_default();
            if content.contains("{\"a\":1}") {
                break;
            }
        }
        assert!(
            content.contains("{\"a\":1}"),
            "flush periódico não chegou ao disco: {content:?}"
        );

        drop(handle);
        t.join().expect("writer encerra");
    }

    /// RNF-PRB-004 — fila cheia descarta e **contabiliza**, sem bloquear.
    #[test]
    fn rnf_prb_004_full_queue_drops_and_counts() {
        // Cria a fila mas nunca roda o writer: tudo satura.
        let (handle, _writer) = writer_channel(Duration::from_secs(5));
        let path = PathBuf::from("nao-usado.csv");

        let mut accepted = 0usize;
        for i in 0..(CAP_WRITE_QUEUE + 64) {
            if handle.append(path.clone(), format!("linha {i}")) {
                accepted += 1;
            }
        }
        assert_eq!(accepted, CAP_WRITE_QUEUE);
        assert_eq!(handle.dropped(), 64);
    }

    /// RNF-PRB-003 — caminho inválido não faz panic; conta erro de I/O.
    #[test]
    fn rnf_prb_003_invalid_path_does_not_panic() {
        let (handle, writer) = writer_channel(Duration::from_millis(50));
        let stats = handle.stats();
        let t = std::thread::spawn(move || writer.run());

        // Um diretório não pode ser aberto como arquivo.
        let dir = tempfile::tempdir().expect("tempdir");
        handle.send(WriteJob::Create {
            path: dir.path().to_path_buf(),
            header: "x".into(),
        });
        drop(handle);
        t.join().expect("writer encerra sem panic");

        assert!(stats.io_errors.load(Ordering::Relaxed) >= 1);
    }
}
