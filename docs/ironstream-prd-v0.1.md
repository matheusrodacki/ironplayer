  
**IRONSTREAM**

MPEG-TS Multicast Player & Stream Analyzer

Product Requirements Document

Versão 0.1  |  **Status: Rascunho**

Plataforma-alvo: Windows 10/11 (x86-64)

Stack: Rust · egui / tauri · FFmpeg (decodificação A/V)

# **1\. Visão Geral**

## **1.1 Contexto**

Ferramentas profissionais de análise de MPEG-TS como VLC, Wireshark e TSReader cobrem casos de uso distintos, mas nenhuma combina em uma única janela: reprodução ao vivo de streams multicast UDP/RTP com visualização em tempo real da estrutura do Transport Stream, das tabelas PSI/SI e DVB, métricas de bitrate por PID e detecção de erros. O IronStream nasce dessa lacuna.

O projeto é desenvolvido em Rust por razões de segurança de memória, performance de parsing de TS em loop de alta frequência e facilidade de distribuição como binário único no Windows sem instalador pesado.

## **1.2 Objetivo do Produto**

Criar um player desktop Windows que:

* Receba streams MPEG-TS via multicast UDP ou RTP/UDP e reproduza o conteúdo de vídeo e áudio com baixa latência.

* Exiba simultaneamente, em painel lateral, a estrutura completa do Transport Stream: tabelas PSI/SI básicas (PAT, PMT) e DVB completas (NIT, SDT, EIT, TDT, BAT).

* Mostre em tempo real o bitrate de cada PID detectado, com gráfico de histórico.

* Detecte e sinalize automaticamente erros de Continuity Counter (CC), jitter de PCR e proporção de null packets.

* Seja empacotado como executável único portável para Windows 10/11 x86-64.

## **1.3 Fora do Escopo (v1.0)**

* Suporte a HLS, DASH ou RTSP.

* Recepção de streams unicast (fase posterior).

* Exportação de capturas para arquivo (fase posterior).

* Plataformas macOS e Linux (roadmap futuro).

* DRM/scrambling — o sistema assume streams livres ou previamente descriptografados.

# **2\. Stakeholders e Personas**

| Persona | Perfil | Objetivo Principal |
| :---- | :---- | :---- |
| Engenheiro de Broadcast | Operação de headend, monitoramento de playout | Verificar estrutura do TS e integridade do sinal multicast em produção |
| Desenvolvedor de Middleware | Implementa EPG, middleware de STB | Inspecionar tabelas DVB (EIT, SDT, NIT) durante desenvolvimento |
| QA / Testador de IPTV | Valida pipelines de encoding e distribuição | Detectar erros de CC e problemas de PCR em ambiente de teste |
| Pesquisador / Estudante | Aprende broadcast digital, DVB-T/C/S | Observar ao vivo o conteúdo das tabelas e entender a estrutura de um TS |

# **3\. Requisitos Funcionais**

## **3.1 Ingestão de Stream**

### **RF-101 — Abertura de stream multicast UDP**

O usuário informa um endereço no formato udp://@239.x.x.x:PORT ou udp://239.x.x.x:PORT. O sistema realiza o join do grupo multicast na interface de rede selecionada e começa a receber pacotes UDP.

| Atributo | Valor |
| :---- | :---- |
| Protocolo de transporte | UDP / RTP over UDP (RFC 3550\) |
| Tamanho do pacote TS | 188 bytes fixos; payload UDP pode conter múltiplos pacotes |
| Buffer de recepção UDP | Configurável, padrão 4 MB |
| Timeout de entrada | Configurável, padrão 5 s; exibe alerta se nenhum pacote chegar |
| Seleção de interface | Dropdown com interfaces de rede disponíveis no sistema |

### **RF-102 — Suporte a RTP**

Quando detectado cabeçalho RTP (primeiros 2 bits \= 10b, PT \= 33 para MPEG-TS), o sistema descarta o cabeçalho RTP (12 bytes \+ CSRC) antes de passar os dados ao parser de TS. O número de sequência RTP é monitorado para detecção de pacotes fora de ordem.

## **3.2 Player de Vídeo e Áudio**

### **RF-201 — Decodificação e renderização de vídeo**

* Decodificação via FFmpeg (libavcodec): H.264, H.265/HEVC, MPEG-2 Video.

* Renderização via backend gráfico nativo Windows (Direct3D 11 texture upload ou OpenGL via wgpu).

* Exibição de resolução, codec, bitrate e frame rate na barra de status do player.

### **RF-202 — Decodificação e reprodução de áudio**

* Decodificação via FFmpeg (libavcodec): AAC, AC-3, MPEG-1 Audio Layer II (MP2), EAC-3.

* Saída de áudio via API nativa Windows (WASAPI).

* Controle de volume e mute na UI.

* Exibição de codec de áudio, taxa de amostragem e número de canais.

### **RF-203 — Seleção de serviço em MPTS**

Em streams MPTS (Multiple Program Transport Stream), o painel de serviços (ver RF-301) permite selecionar qual serviço será apresentado no player. A troca de serviço deve ocorrer em até 2 segundos.

### **RF-204 — Latência de reprodução**

O atraso fim-a-fim (do recebimento do pacote UDP à exibição do frame correspondente) deve ser inferior a 2 segundos em condições normais de rede, configurável via buffer de jitter.

## **3.3 Analisador de Transport Stream**

### **RF-301 — Visão geral de PIDs**

Painel dedicado exibindo em tempo real uma tabela com todos os PIDs detectados, com as seguintes colunas:

| PID (hex) | Tipo | Descrição / Serviço | Bitrate (kbps) | Erros CC |
| :---- | :---- | :---- | ----- | ----- |
| 0x0000 | PAT | Program Association Table | — | — |
| 0x0100 | Vídeo (H.264) | Serviço: Canal 1 | 3.840 | 0 |
| 0x0101 | Áudio (AAC) | Serviço: Canal 1 | 192 | 0 |
| 0x1FFF | Null Packet | Preenchimento de banda | — | — |

### **RF-302 — Tabelas PSI básicas (PAT e PMT)**

* PAT (PID 0x0000): exibir lista de programas com seus respectivos PMT PIDs e Program Numbers.

* PMT (por programa): exibir PCR PID, lista de elementary streams com PID, stream type (hex \+ descrição legível) e descriptors associados.

* Ao clicar em um programa na PAT, a PMT correspondente é exibida em subpainel ou aba.

* Indicar versão da seção e ciclo de repetição (tempo entre duas versões consecutivas idênticas).

### **RF-303 — Tabelas DVB (NIT, SDT, EIT, TDT, BAT)**

* NIT (PID 0x0010): network\_id, network\_name (via network\_name\_descriptor), lista de transport streams com descriptors (satellite delivery, cable delivery, etc.).

* SDT (PID 0x0011): por serviço — service\_id, service\_name, provider\_name, service\_type, running\_status, EIT flags.

* EIT (PID 0x0012): grade de programação atual/seguinte (EIT p/f) e schedule (EIT sched) quando presente. Exibir event\_name, start\_time (convertido para horário local), duration, short\_event\_description.

* TDT (PID 0x0014): exibir UTC time decodificado em formato legível; calcular diferença em relação ao relógio do sistema.

* BAT (PID 0x0011 com table\_id 0x4A): bouquet\_id, bouquet\_name, lista de serviços por bouquet.

* Sinalizar tabelas ausentes (ex.: stream sem NIT) com indicador visual.

### **RF-304 — Bitrate por PID em tempo real**

* Calcular bitrate em janela deslizante de 1 segundo para cada PID ativo.

* Exibir gráfico de linha com histórico de 60 segundos por PID selecionado.

* Exibir bitrate total do stream e proporção de null packets (overhead/padding %).

* Painel de barras horizontais com top-N PIDs por bitrate (N configurável, padrão 10).

### **RF-305 — Detecção de erros**

| Tipo de Erro | Descrição | Indicação na UI |
| :---- | :---- | :---- |
| Continuity Counter (CC) | Incremento inválido do CC (0–15 circular) em qualquer PID não-null | Contador por PID \+ alerta visual em vermelho |
| PCR Jitter | Variação do PCR além de ±500 µs entre pacotes consecutivos do mesmo PID | Gráfico de jitter \+ alerta quando exceder limiar |
| PCR Discontinuity | Flag PCR\_discontinuity\_indicator presente ou salto \> 100 ms sem flag | Log de eventos com timestamp |
| Null Packet Ratio | Proporção de PID 0x1FFF \> 30% do total de pacotes | Indicador colorido (verde/amarelo/vermelho) |
| Pacotes duplicados RTP | Número de sequência RTP repetido | Contador no painel RTP |
| Buffer underrun UDP | Sistema operacional reporta perda de pacotes UDP | Alerta persistente na barra de status |

# **4\. Requisitos Não Funcionais**

| Requisito | Critério de Aceitação |
| :---- | :---- |
| Desempenho de parsing | Parser de TS processa ≥ 200 Mbps em thread dedicada sem afetar a UI (CPU \< 20% em i5 de geração 10+) |
| Latência da UI | Painel de PIDs atualiza a cada 1 s; nenhuma tela congelada \> 100 ms |
| Memória | Consumo máximo de 256 MB em operação normal com 1 stream ativo |
| Inicialização | Pronto para receber stream em \< 3 s após lançamento do executável |
| Distribuição | Binário único portável, sem necessidade de instalador; FFmpeg vinculado estaticamente ou distribuído como DLL alongside |
| Compatibilidade Windows | Windows 10 (21H2+) e Windows 11, x86-64 |
| Segurança de memória | Zero uso de unsafe Rust fora de bindings FFI explicitamente documentadas |
| Recuperação de erros | Perda de sinal de rede não deve travar o processo; reconexão automática após timeout configurável |

# **5\. Arquitetura de Alto Nível**

## **5.1 Visão Geral dos Componentes**

O IronStream é organizado em três camadas principais: Network Layer, Processing Pipeline e UI Layer. A comunicação entre as camadas é feita via canais assíncronos (tokio::mpsc / crossbeam), garantindo que nenhum processamento de rede bloqueie a thread da UI.

| Componente | Responsabilidade |
| :---- | :---- |
| network::receiver | Socket UDP multicast (via socket2 crate); suporte a join de grupo multicast IPv4/IPv6; desmontagem de cabeçalho RTP opcional; envia buffers brutos ao pipeline |
| ts::demuxer | Valida sync byte (0x47), extrai PID, payload\_unit\_start\_indicator, adaptation field e payload; mantém estado de CC por PID; alimenta seção assembler e AV queue |
| ts::section\_assembler | Reagrupa seções TS (que podem se fragmentar em múltiplos pacotes); filtra por table\_id; entrega seções completas ao parser de tabelas |
| ts::table\_parser | Implementação própria de PAT, PMT, NIT, SDT, EIT, TDT, BAT; desserializa structs Rust tipadas; calcula CRC-32 MPEG |
| ts::pcr\_tracker | Monitora PCR por PID; calcula jitter e detecta descontinuidades |
| ts::bitrate\_meter | Janela deslizante de 1 s por PID; cálculo de bitrate total e proporção de null packets |
| av::ffmpeg\_bridge | FFI para libavcodec/libavformat; recebe PES packets do demuxer; decodifica vídeo e áudio; envia frames decodificados ao renderer |
| av::video\_renderer | Textura do frame de vídeo via wgpu (backend D3D11 no Windows); integrado ao widget egui |
| av::audio\_output | Saída de áudio via cpal crate (usa WASAPI no Windows); buffer de jitter configurável |
| ui::app | Janela principal egui/eframe; layout em três painéis: player, árvor de TS, painel de métricas |
| ui::pid\_panel | Tabela reativa de PIDs com ordenação por coluna |
| ui::tables\_panel | Árvore expansível com PAT → PMT → streams; abas para NIT, SDT, EIT, TDT, BAT |
| ui::metrics\_panel | Gráficos de bitrate (egui\_plot) e painel de erros |

## **5.2 Crates Rust Planejados**

| Crate | Uso |
| :---- | :---- |
| tokio | Runtime assíncrono para tarefas de rede e pipeline |
| socket2 | Controle fino de socket UDP multicast (SO\_REUSEADDR, IP\_ADD\_MEMBERSHIP) |
| bytes / byteorder | Buffer zero-copy e leitura de inteiros big-endian para parsing de TS |
| eframe / egui | Framework de UI imediata, cross-platform, compilado junto ao binário |
| egui\_plot | Gráficos de bitrate e PCR jitter dentro do egui |
| wgpu | Renderização de frames de vídeo como textura GPU |
| cpal | Abstração de saída de áudio (WASAPI no Windows) |
| ffmpeg-next (ffmpeg-sys-next) | Bindings seguras para libavcodec / libavformat |
| crossbeam-channel | Canais de alta performance entre threads de parsing e UI |
| serde / serde\_json | Serialização de configurações e exportação futura de dados |
| tracing / tracing-subscriber | Logs estruturados e diagnóstico |
| anyhow / thiserror | Tratamento de erros ergonômico |

## **5.3 Estratégia com FFmpeg**

O FFmpeg é utilizado exclusivamente para decodificação de vídeo e áudio (componente av::ffmpeg\_bridge). Todo o parsing de TS, tabelas PSI/SI e DVB é implementado em Rust puro, sem depender do libavformat para demux. Isso permite:

* Parsing mais granular e tipado das tabelas DVB sem contornar as abstrações do FFmpeg.

* Controle total sobre a detecção de erros de CC e PCR que o FFmpeg não expõe via API pública.

* Independência de versão do FFmpeg para a lógica de análise.

Distribuição no Windows: as DLLs do FFmpeg (avcodec, avutil, swresample, swscale) são distribuídas junto ao executável na mesma pasta. Nenhuma instalação global é necessária.

# **6\. Design da Interface**

## **6.1 Layout Principal**

A janela principal é dividida em três regiões:

| Região | Conteúdo |
| :---- | :---- |
| Painel esquerdo (40%) | Player de vídeo com controles de volume; barra de status (codec, resolução, bitrate de vídeo, latência estimada); campo de URL e botões Conectar / Desconectar |
| Painel central (35%) | Aba 'PIDs': tabela de PIDs com bitrate e erros. Aba 'Tabelas': árvore expandível com PAT, PMT e tabelas DVB. Aba 'Serviços': lista de serviços para MPTS |
| Painel direito (25%) | Gráficos de bitrate por PID selecionado; gráfico de PCR jitter; painel de erros com log de eventos |

## **6.2 Tema e Acessibilidade**

* Tema escuro padrão (adequado para ambientes de operação de broadcast com pouca luz).

* Opção de tema claro nas configurações.

* Tamanho mínimo de fonte: 12px. Interface totalmente redimensionável.

* Indicadores de erro usam combinação de cor \+ ícone (não apenas cor) para acessibilidade.

## **6.3 Barra de Status Global**

Barra persistente no rodapé exibindo: estado da conexão (Conectado / Desconectado / Erro), bitrate total do stream em Mbps, contagem de erros de CC acumulados, diferença TDT vs. relógio local, e versão do aplicativo.

# **7\. Casos de Uso Principais**

## **UC-01 — Monitorar stream multicast em produção**

1. Engenheiro abre o IronStream; seleciona a interface de rede correta no dropdown.

2. Insere o endereço multicast no formato udp://@239.1.1.1:1234 e clica em Conectar.

3. O player começa a reproduzir o vídeo em 2–5 segundos; o painel de PIDs é preenchido automaticamente.

4. Engenheiro verifica a tabela SDT para confirmar o nome do serviço e abre a EIT para checar o programa atual.

5. Ao detectar um aumento de erros de CC no PID de vídeo, o engenheiro clica no PID para ver o gráfico de histórico de bitrate.

## **UC-02 — Desenvolver e testar tabelas DVB personalizadas**

6. Desenvolvedor conecta ao stream de teste; navega até a aba Tabelas.

7. Expande o nó NIT para verificar se os descriptors de entrega via cabo estão corretos.

8. Verifica SDT para confirmar service\_type e running\_status após deploy de nova versão do middleware.

9. Compara horário TDT com relógio local para validar sincronismo de tempo.

## **UC-03 — Detectar problema de qualidade em pipeline de encoding**

10. QA enginner conecta ao stream de saída do encoder.

11. Observa no painel de erros um aumento periódico de PCR jitter no PID 0x0100.

12. Correlaciona o painel de bitrate com o gráfico de jitter: picos de bitrate causam o jitter.

13. Exporta (via copy para clipboard) o log de erros com timestamps para incluir no bug report.

# **8\. Fases de Desenvolvimento**

| Fase | Entregável | Critério de Conclusão |
| :---- | :---- | :---- |
| Alpha (v0.1) | Recepção UDP multicast \+ parser TS \+ tabela de PIDs \+ PAT/PMT | Stream funcional recebido e PIDs exibidos; PAT/PMT parseados |
| Alpha (v0.2) | Integração FFmpeg: decodificação de vídeo H.264 e áudio AAC | Vídeo e áudio reproduzidos em janela egui |
| Beta (v0.3) | Tabelas DVB completas: NIT, SDT, EIT, TDT, BAT | Todas as tabelas parseadas e exibidas na UI para um stream DVB-C/T real |
| Beta (v0.4) | Bitrate por PID em tempo real \+ gráfico de histórico | Gráficos atualizando a cada 1 s; bitrate total correto ±5% |
| Beta (v0.5) | Detecção de CC, PCR jitter e null packet ratio | Erros gerados artificialmente são detectados em \< 2 s |
| RC (v0.9) | Suporte a RTP \+ seleção de serviço MPTS \+ tema claro/escuro \+ portabilidade | Binário único portável testado em Windows 10 e 11 limpos |
| v1.0 | Estabilização, docs, release público | Zero crash em 8 h de operação contínua; docs de usuário completas |

# **9\. Critérios de Aceite da v1.0**

* Conectar a um stream multicast UDP com TS mono-programa (SPTS) em até 5 segundos.

* Conectar a um stream MPTS e listar corretamente todos os serviços presentes.

* Exibir PAT e todas as PMTs de um MPTS sem erros de parsing.

* Exibir NIT, SDT, EIT (p/f e schedule), TDT e BAT de um stream DVB-C real.

* Detectar 100% dos erros de CC introduzidos artificialmente via ferramenta de injeção de erros.

* Detectar jitter de PCR superior a 1 ms em 95% das ocorrências.

* Bitrate reportado por PID com desvio máximo de ±5% em relação ao bitrate medido por ferramenta de referência (TSReader ou tsduck).

* Executável único de no máximo 60 MB (incluindo DLLs FFmpeg) rodando em Windows 10/11 sem instalação adicional.

* Nenhum crash ou vazamento de memória acima de 10 MB/h em operação contínua de 8 horas.

# **10\. Riscos e Mitigações**

| Risco | Probabilidade / Impacto | Mitigação |
| :---- | :---- | :---- |
| Bindings FFmpeg para Rust (ffmpeg-next) podem ter limitações com versões mais recentes do FFmpeg | Média / Alto | Fixar versão do FFmpeg (6.x LTS); testar build reproduzível com cargo build \--locked |
| Rendering de vídeo via wgpu \+ egui pode ter problemas em GPUs antigas sem suporte D3D11 adequado | Baixa / Alto | Fallback para rendering via CPU (swscale → bitmap) como opção de compatibilidade |
| Parsing de tabelas DVB privadas ou com extensões proprietárias pode causar panic no parser | Alta / Médio | Tratar seções desconhecidas com fallback gracioso; nunca usar unwrap() em dados externos |
| Streams com bitrate muito alto (\> 200 Mbps, ex.: UHDTV) podem saturar o canal de dados entre threads | Baixa / Médio | Backpressure no canal mpsc; dropping de frames de análise com contador de drops visível na UI |
| Multicast em redes com IGMP snooping pode exigir configuração manual do switch | Alta / Baixo | Documentar o processo de join e fornecer ferramenta de diagnóstico de membros IGMP no painel de rede |

# **11\. Glossário**

| Termo | Definição |
| :---- | :---- |
| BAT | Bouquet Association Table — tabela DVB que agrupa serviços em bouquets de operadora |
| CC | Continuity Counter — campo de 4 bits em cada pacote TS que incrementa ciclicamente para detectar perdas |
| EIT | Event Information Table — tabela DVB com grade de programação (EPG) |
| MPTS | Multiple Program Transport Stream — TS contendo mais de um programa/serviço |
| NIT | Network Information Table — tabela DVB com informações de rede física e lista de transport streams |
| PAT | Program Association Table — tabela PSI que mapeia Program Numbers para PMT PIDs |
| PCR | Program Clock Reference — referência de clock inserida no TS para sincronização de decodificadores |
| PES | Packetized Elementary Stream — encapsulamento de bitstream de vídeo/áudio dentro do TS |
| PID | Packet Identifier — campo de 13 bits que identifica o tipo de cada pacote no TS (0x0000–0x1FFF) |
| PMT | Program Map Table — tabela PSI que descreve os componentes (PIDs) de cada programa |
| PSI | Program Specific Information — conjunto de tabelas normativas do MPEG-2 Systems (PAT, PMT, CAT, NIT) |
| RTP | Real-time Transport Protocol — protocolo de transporte para mídia em tempo real sobre UDP |
| SDT | Service Description Table — tabela DVB com nome, tipo e status dos serviços |
| SPTS | Single Program Transport Stream — TS com apenas um programa |
| TDT | Time and Date Table — tabela DVB que transporta o horário UTC da rede |
| TS | Transport Stream — formato de contêiner MPEG-2 Systems; pacotes de 188 bytes com sync byte 0x47 |

