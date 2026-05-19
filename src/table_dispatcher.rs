/// SPEC-TABLE — Stub mínimo do TableDispatcher para o bootstrap da Task 3.
///
/// Recebe [`CompleteSection`] do `SectionAssembler`, roteia por `table_id` e
/// emite [`TableEvent`] para o `AppState`.
///
/// **Status:** stub de bootstrap. Implementação completa será feita quando o
/// AppState e a UI estiverem disponíveis.
use crossbeam_channel::Receiver;
use tracing::trace;
use ts::CompleteSection;

use crate::channels::{BoundedSender, TableEvent};

/// SPEC-TABLE
/// Despacha seções PSI/SI completas para o `AppState`.
pub struct TableDispatcher {
    rx: Receiver<CompleteSection>,
    tx: BoundedSender<TableEvent>,
}

impl TableDispatcher {
    /// Cria um novo `TableDispatcher`.
    pub fn new(rx: Receiver<CompleteSection>, tx: BoundedSender<TableEvent>) -> Self {
        Self { rx, tx }
    }

    /// Loop principal: drena `complete_sections` e despacha `TableEvent`.
    ///
    /// Termina quando o sender do canal `complete_sections` é fechado.
    pub fn run(self) {
        for section in self.rx.iter() {
            trace!(
                pid = section.pid,
                table_id = section.table_id,
                bytes = section.data.len(),
                "seção recebida"
            );
            // TODO: parsear por table_id e popular AppState.
            // Por ora, emite um TableEvent placeholder para manter o canal ativo.
            self.tx.try_send(TableEvent);
        }
    }
}
