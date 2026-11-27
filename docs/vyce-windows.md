# Desenvolvimento local do Vyce no Windows

O Vyce usa o código do fork Belobug como ponto de partida. Os resultados do
README foram medidos pelo Belobug no PC dele.

## Comandos

Execute estes comandos em PowerShell a partir da raiz do Vyce.

~~~powershell
.\scripts\vyce.ps1
.\scripts\vyce.ps1 -Action Build
.\scripts\vyce.ps1 -Action Build -Profile diag
~~~

A checagem padrão mostra os caminhos e o estado do binário. A compilação
usa o Cargo.lock e gera victory.exe fora do repositório. O perfil diag
mantém uma segunda compilação com as ferramentas de diagnóstico habilitadas.

A compilação é offline: o launcher põe CARGO_NET_OFFLINE=true, e o Cargo
só usa as dependências que já estão em Vyce-data\cargo (se faltar alguma,
ele para e o launcher avisa). Com -Online o Cargo pode baixar o que
faltar:

~~~powershell
.\scripts\vyce.ps1 -Action Build -Online
~~~

Quando quiser iniciar o jogo de propósito:

~~~powershell
.\scripts\vyce.ps1 -Action Run
~~~

O launcher usa Series X por padrão. Selecione -Profile diag para executar a
compilação de diagnóstico. O stdout e o stderr ficam em arquivos separados
na pasta logs. O argumento do executável também continua disponível para quem
usa a CLI diretamente. Quando o argumento é omitido, o arquivo é localizado
na pasta indicada por game_dir no victory.toml.

### Opções do jogo

O launcher ajusta o arquivo de configurações do próprio jogo antes de
abri-lo (Vyce-data\data\<título>\storage\settings\settings.xml); a
primeira cópia fica ao lado como settings.xml.original, e a usada em cada
sessão vai para logs\<data>.settings.xml.

| Opção | Efeito |
| --- | --- |
| -OutputSize 1920x1080 | Tamanho da imagem do jogo, no arquivo e no código do jogo (abaixo); keep deixa os dois como estão |
| -RenderScale 0.5 a 1.0 | Parte da largura e da altura da imagem que a cena tem; sem ela, os 2/3 do próprio jogo (cena de 1280x720 em 1920x1080); 1.0 dá cena de 1920x1080 |
| -NoTitlePatch | Não muda o código do jogo (tamanho, escala e upscaler ficam os dele); o arquivo continua sendo escrito |
| -ConsoleProfile series-s | Perfil do Series S, com o upscaler do próprio jogo no lugar do FidelityFX |
| -TitleSettings light | Desliga o que mais pesa numa placa de PC: filas assíncronas, borrão de movimento, menos detalhe ao longe e reflexos menores |
| -TitleSettings keep | Não muda nada além do tamanho |
| -TitleSettings original | Volta o arquivo como o jogo o criou |
| -TitleArguments a, b | Argumentos repassados ao jogo |

Ainda não se viu o jogo respeitar cada valor do arquivo: compare uma
sessão com -OutputSize keep -TitleSettings keep e procure no log as linhas
"presenting to a window" e "twins made".

### Tamanho da imagem no código do jogo

No perfil Series X o jogo ignora o tamanho do arquivo: monta uma cena de
2560x1440 numa imagem de 3840x2160. Por isso o launcher também muda o
código do jogo ao carregá-lo, antes de qualquer parte dele rodar:

| Variável | Efeito |
| --- | --- |
| VICTORY_OUTPUT=1920x1080 | Tamanho da imagem: um dos modos do jogo (1280x720, 1664x936, 1920x1080, 2560x1440, 3840x2160). Vem de -OutputSize |
| VICTORY_RENDER_SCALE=0.5 a 1.0 | Escala da cena no upscaler do próprio jogo (2/3 sem ela; abaixo de 2/3 o maior tamanho de cena do jogo fica em 2/3). Vem de -RenderScale |
| VICTORY_UPSCALER=6 ou 7 | Upscaler escolhido: 6 o do jogo, 7 o FidelityFX. O launcher põe 6 no Series S, que sozinho escolhe o FidelityFX (outros forks na NVIDIA viram o mundo preto com ele) |

Cada lugar do código é comparado byte a byte antes: só a versão 1.0.2.1
(blake3 b99f876b...) tem esses bytes. Se um lugar pedido difere, nenhum é
mudado, e o log diz "resolution: not patched"; com tudo certo ele diz
"resolution: patched" para cada lugar. Uma variável já definida no
processo que chama o launcher vale no lugar da do launcher
(VICTORY_OUTPUT=off deixa o tamanho como o jogo quer). O FidelityFX tem
uma escala própria no fork de origem (VICTORY_FFX_SCALE), que não veio:
em 0,5 o jogo dividiu por zero 49 s depois.

Imagem de 1920x1080 em vez de 3840x2160: a cópia para a janela, a
composição das duas camadas do Series S, o FPS no canto e o tamanho da
janela seguem o tamanho que o jogo apresenta.

### Cache pronto antes de jogar

Uma mudança no tradutor ou no driver faz o jogo traduzir os shaders e
montar os pipelines de novo na primeira sessão, o que deixa os primeiros
minutos lentos. O comando warm faz isso antes, com o jogo fechado:

~~~powershell
victory warm D:\torrent\Vyce-data\lab\shaders\host --pipelines D:\torrent\Vyce-data\lab\compute-used.txt
~~~

Ele traduz os contêineres das pastas dadas para o cache de shaders e monta
os pipelines de compute cujos hashes o arquivo lista. Pipelines gráficos
dependem do estado que o jogo passa e só são montados em jogo.

Rode-o com o mesmo ambiente que o launcher dá ao jogo (VICTORY_CONFIG,
VICTORY_CACHE_DIR e VICTORY_DATA_DIR de Vyce-data) e com as chaves do
tradutor como o jogo vai usar: uma chave trocada é outro cache. O alvo da
tradução vem da placa, como no jogo; o log diz "host wave sizes" (32 e 32
lanes, exact_views=true na RTX 2070 SUPER) e "the host's view heaps hold
fewer views than the title's" (views=1000000). No fim ele diz quantas
traduções vieram do cache e quantas foram feitas agora, e quantos
pipelines foram compilados agora e quantos vieram da biblioteca. A lista
dos pipelines do jogo que começam cedo no carregamento
(cache\pipelines\early-pipelines.txt) fica como estava: até a build B o
warm a gravava vazia ao salvar a biblioteca.

Só a falta desse arquivo conta como lista vazia. Se ele existe e não pode
ser lido (outro programa o segura, falta permissão), o log avisa uma vez
("the list of title pipelines to start early cannot be read: it is not
written over until it can be"), nada começa cedo por ele, e cada
salvamento grava a biblioteca mas deixa o arquivo como está; os pipelines
que a sessão usou ficam guardados e entram na lista no primeiro
salvamento que conseguir lê-la. Antes, qualquer erro de leitura valia
como lista vazia, e o salvamento seguinte gravava por cima dela só o que
a sessão tinha usado. Bytes que não são texto num arquivo danificado
viram chaves que não casam com nenhum pipeline; as outras continuam
valendo.

## Local dos arquivos

Com o clone em D:\torrent\Vyce, o launcher usa a pasta irmã
D:\torrent\Vyce-data. O projeto foi configurado com a seguinte estrutura:

| Pasta em Vyce-data | Conteúdo |
| --- | --- |
| cache | Shaders e outros dados derivados, separados dos saves |
| data | Saves e espaço temporário do título |
| logs | Saída do launcher e da compilação inicial |
| temp | Temporários do processo e das ferramentas de compilação |
| cargo | Dependências do Cargo |
| target | Binários, símbolos e artefatos de compilação |
| toolchain | Rust 1.98.1, linker LLD e bibliotecas Windows x64 |
| packages | Pacotes de origem usados para a preparação local |

O victory.toml local é ignorado pelo Git. Os arquivos do jogo ficam em sua
própria pasta. O launcher aplica as variáveis de ambiente ao processo e as
restaura ao sair. A configuração do Windows e a paginação são independentes.

A compilação portátil usa o LLVM do Rust, as bibliotecas do Windows SDK x64
do NuGet e o CRT estático das ferramentas v143 da Microsoft. A opção
blake3/pure permite compilar a dependência sem um compilador C.

## Comparar desempenho

O caminho de heap limitado reserva 262.144 descritores dinâmicos para as
tabelas de desenho. O limite total de um milhão continua valendo. O espaço
extra para tabelas reduz a faixa de índices que o título pode usar
diretamente. Para comparar com a divisão anterior, defina
VICTORY_CAPPED_DYNAMIC=65536 no processo que chama o launcher.

Os descritores dinâmicos das tabelas de uma lista voltam assim que a GPU
termina a última submissão dela (antes voltavam só quando o jogo reiniciava
ou soltava a lista, o que numa sessão no mundo chegou a 249.113 ocupados e
uma vez faltou). Isso vale para as listas que o jogo já mostrou usar uma vez
por gravação (ele reiniciou uma gravação dela enviada uma vez só); a
primeira gravação de cada lista guarda os descritores até reiniciar, como
antes, e uma lista que o jogo enviou de novo sem reiniciar passa a guardá-los
assim em todas as gravações. Os descritores devolvidos ficam emprestados:
outra tabela só os toma quando não acha outros livres (campo lent_taken). Se
o jogo enviar de novo uma gravação que já rodou, ela pega de volta os mesmos
descritores; se outra tabela já tiver
tomado algum, a lista inteira (todas as partes dela) roda sem os seus
comandos nesse envio e em todos os seguintes, até o jogo reiniciar a lista
(aviso "a list submitted again after other tables took its descriptor slots
runs without its commands until its reset", campo tables_lost); as escritas
de fim de lista e as transições das texturas continuam. O aviso "a list
submitted again keeps its descriptor tables until its reset" diz que o jogo
envia gravações de novo. Uma tabela que não acha descritores livres espera
a submissão mais antiga em voo terminar e tenta de novo uma vez antes de ser
descartada.

O limite padrão de loops dos shaders permanece o mesmo. Após aprovar o
próximo teste, pode-se escolher um limite explícito com
-ShaderLoopLimit 4096. O cache distingue o limite escolhido. Valores menores
podem cortar trabalho válido do shader e mudar a imagem. Compare a mesma
cena e um cache aquecido, verificando imagem, comandos desenhados, tempo
por quadro e os novos campos ms e prepare_ms do aviso de submissão lenta.

## Painel de diagnóstico

A ação Run abre o jogo e, na borda direita da tela, o painel do Vyce, sem
tirar o foco do jogo. A parte de cima mostra o momento:

- o FPS, o tempo de cada quadro, há quanto tempo veio a última medida e um
  gráfico dos últimos 2 minutos. Sem quadro novo por 4 s, o número fica
  cinza e o painel diz há quanto tempo o jogo não apresenta nada. No
  gráfico, 1, 10 e 100 FPS ficam à mesma distância, para poucos quadros por
  segundo não sumirem ao lado das centenas de um menu;
- a faixa de carregamento: etapa, porcentagem, tempo que falta e o que o
  carregamento espera (o disco ou pipelines), das linhas VYCE_LOADING. Sem
  essas linhas, a etapa é deduzida dos quadros, sem porcentagem. Se o log
  inteiro para por 10 s, a faixa avisa que o jogo pode estar parado;
- para onde vai o tempo do quadro: GPU, preparo na CPU, cópia de volta e o
  resto, numa barra, com uma frase sobre quem segura o quadro. O mouse
  sobre cada parte mostra os detalhes. Embaixo, os números de como o
  trabalho chega à GPU que a build informar: envios e chamadas
  ExecuteCommandLists, esperas pela GPU, sinais adiados, syncs, desenhos
  indiretos e desenhos deixados de fora;
- VRAM usada e o limite (e quanto vai em texturas e nas imagens em que o
  jogo desenha, se a build informar), RAM, processador, limite de textura,
  desenhos e descartados por quadro, pipelines na fila e buffers na VRAM.

Campos de VYCE_TELEMETRY que a build não tem ficam de fora; os que o painel
não conhece aparecem, com o nome de origem, em Copiar resumo.

Embaixo, Resumo lista os eventos em português, com as repetições juntas e
contadas: etapas, avisos e erros. Um clique em um evento mostra o que ele
quer dizer e as linhas do log de onde veio. Técnico mostra o log limpo, com
filtro; o filtro também acha as linhas repetitivas, e Mostrar tudo inclui
essas linhas e as de telemetria.
Copiar resumo põe na área de transferência um texto curto para colar numa
conversa: placa, versão, medianas do último minuto, avisos por tipo e os
últimos erros. Quando o jogo termina, o painel diz como (janela fechada,
placa de vídeo parou, travamento, código de saída) e continua aberto.

O painel atualiza CPU e RAM do sistema e do jogo a cada segundo. A RAM do
sistema mostra uso, total e memória disponível. A RAM do jogo corresponde
à memória residente. A CPU do jogo é medida como fração da capacidade
total da máquina. O painel também mostra o número de processadores
lógicos e de threads do jogo.

Use -NoDebugWindow para abrir só o jogo. Fechar o painel mantém a sessão
do jogo aberta. VYCE_PANEL_BOUNDS=x,y,largura,altura abre o painel nesse
lugar. A parte do painel escrita em C# é compilada na primeira vez e
guardada na pasta temporária; depois o painel abre em cerca de um segundo.
Se ela não compilar, abre um painel simples com os números e os avisos;
VYCE_PANEL=simples abre esse painel de propósito. O arquivo .monitor.log ao
lado dos logs guarda as falhas do próprio painel e quanto tempo ele levou
para ler o log na abertura.

Os dois registros continuam em Vyce-data\logs. Nas builds com telemetria,
o arquivo de erros também recebe uma linha VYCE_TELEMETRY por amostra para
permitir comparações posteriores. O painel lê os registros desde o começo,
então pode ser aberto a qualquer momento.

A telemetria separa o trabalho da GPU da espera por ela. Estes campos de
VYCE_TELEMETRY são por quadro, exceto os de memória:

| Campo | Significado |
| --- | --- |
| gpu_busy_ms, gpu_timed_per_frame | Tempo medido na própria GPU, do início ao fim de cada submissão, e quantas foram medidas |
| submissions_per_frame, unwaited_per_frame | Submissões, e as do jogo que voltaram antes de a GPU terminar |
| held_by_card_per_frame | Submissões do jogo que esperaram só porque shaders escrevem em buffers da VRAM |
| snapshot_ms, steps_ms, card_record_ms | Partes do preparo: cópias de referência, trabalho adiado pelas listas, buffers da VRAM |
| submit_ms | Entrega das listas à fila (parte da espera pela GPU nas submissões esperadas) |
| prewarm_ms | Cópias adiantadas enquanto a submissão anterior roda (fora do preparo) |
| vram_mb, vram_budget_mb, shared_mb, card_mb | Memória de vídeo usada e o limite do Windows, memória compartilhada, buffers na VRAM |

O relógio da GPU acrescenta duas listas curtas a cada submissão. Ele só
existe quando a telemetria está ligada.

Para comparar sem mudar de binário, defina no processo que chama o launcher
VICTORY_ASYNC_SUBMIT=0 (submissões esperadas, como antes),
VICTORY_STRETCH=0 (sem a memória do jogo usada no lugar por uma segunda
vista) ou VICTORY_PLAN_REUSE=0 (plano de descritores anterior).

Chaves de comparação para a placa NVIDIA (no processo que chama o
launcher):

| Variável | Efeito |
| --- | --- |
| VICTORY_EXACT_VIEWS=0 | Volta às vistas de buffer de antes (a NVIDIA lê outros bytes de vistas estruturadas de passo múltiplo de 8, 12 ou 16; medido numa RTX 2070 SUPER) |
| VYCE_SYNC_BARRIER=0 | As sincronizações do jogo voltam a ser ignoradas (hoje viram uma barreira para as escritas dos shaders) |
| VICTORY_WRITE_SKIP=0 | Toda vista de escrita é tratada como escrita, mesmo de shaders que não escrevem em buffers |
| VICTORY_HAIR=1, VICTORY_RAY_RESOLVES=1 | Rodam os fios de cabelo e os resolves de raios, deixados de fora (os resolves não têm o que resolver sem raios e custam de 10 a 30 s de compilação cada) |
| VICTORY_HOIST_CLAMPS=7 | Os limites dos argumentos indiretos rodam um após o outro, como antes (o padrão agora é 1) |
| VICTORY_EXACT_NORM=0 | Códigos normalizados (UNORM/SNORM) de buffers voltam a ser divididos pela GPU, que erra o último bit em 126 dos 256 códigos de 8 bits nesta placa (materiais trocados) |
| VICTORY_D16_INTEGER=0 | Dados de 16 bits de vistas de formato inteiro voltam a passar por f16 (viravam ruído) |
| VICTORY_WAVE_FORMS=0 | Programas wave64 recusados não são tentados de novo sem os saltos de saída antecipada |
| VICTORY_WAVE_PAIRS=0 | Programas wave64 de compute que nenhuma outra forma aceita voltam a ser recusados (os passes deles são descartados), em vez de rodar em pares de lanes: uma onda de 32 da placa para cada onda de 64 do console, cada thread com duas lanes do console |
| VICTORY_LDS_LOCAL=0 | Grupos de uma onda com memória compartilhada voltam ao fluxo do console (com uma barreira em cada troca) em vez de duas ondas locais de 32 |
| VICTORY_LOCAL_MEET_CHECKS=0 | As duas ondas locais voltam a se encontrar onde o fork as punha: também dentro de laços que cada thread deixa numa volta diferente, dentro de desvios em valores que as duas ondas podem ter diferentes (leitura de uma lane, inclusive de um acumulador como `v_fmac_f32` ou de uma seleção por máscara constante, e leitura escalar de memória escrita enquanto o dispatch roda) e em programas que acessam memória por endereço. Só para comparação: pode travar a GPU |
| VICTORY_LOOP_EARLY_OUTS=0 | As formas sem saída antecipada voltam a tirar também a saída de dentro de um laço, mesmo quando é a única saída dele (as threads giram até o limite de voltas). Só para comparação: pode travar a GPU |
| VICTORY_HOST_WAVE_READS=0 | Com as checagens acima ligadas, a análise das duas ondas locais volta a ser a de 1952aaf: um valor conta como igual em todas as lanes mesmo quando a instrução lê algo além dos operandos (o valor antigo de um acumulador como `v_fmac_f32`, `v_mac_f32` ou `v_dot4c_i32_i8`, o de uma troca, o VCC de `v_div_fmas_f32`) ou seleciona por uma máscara constante de algumas lanes, e uma leitura escalar conta como igual nas duas ondas mesmo de memória escrita enquanto o dispatch roda. Só para comparação: pode travar a GPU |
| VICTORY_VIEW_LOADS_APART=0 | Com VICTORY_HOST_WAVE_READS ligada, uma leitura escalar simples (`s_load_*`, `s_buffer_load_*`, sem glc nem dlc) de uma vista de acesso não ordenado ou sem tipo declarado volta a contar como algo que as duas ondas locais podem ler diferente só num programa que escreve na memória, como na 5c99db2. Hoje conta em qualquer programa: outro dispatch pode escrever nessa vista entre as duas leituras. Só para comparação: pode travar a GPU |
| VICTORY_LANE_READ_BRANCHES=0 | Um programa com um desvio que as duas ondas locais tomariam diferente só por uma leitura de lane (`v_readfirstlane_b32`, `v_readlane_b32`) de um valor que as lanes têm diferente (que não seja a base reservada pelo idioma de alocação) volta a rodar como ondas locais antes do fluxo do console. Ali cada onda local lê uma lane sua, e o programa pode seguir outro caminho que o do console (no teste LDS_FMAC, 32 em vez de 64). Hoje ele vai para o fluxo do console; se o fluxo do console não o traduz, fica com as ondas locais de antes. Não conta a leitura da primeira lane de um valor que toda lane tem igual: uma leitura de memória num endereço igual em todas as lanes, ou o índice do grupo tirado do índice da thread (`(grupo << 6 + t) >> 6`) |
| VICTORY_STRIDE_ONCE=0 | A tabela de passos volta a ser lida a cada uso, não uma vez por vista e bloco |
| VICTORY_INTERP_F16_MODS=0 | Interpolações de 16 bits voltam a escrever só a metade baixa, sem multiplicador nem saturação |
| VICTORY_F16_NEAREST=1 | Resultados de 16 bits arredondados ao mais próximo, como no console (a Turing arredonda para zero); desligado por padrão, custa uma dúzia de instruções por resultado |

Cada uma dessas chaves do tradutor muda a chave do cache de shaders: com
uma delas trocada, todos os shaders são traduzidos de novo (o log avisa).

Chaves de comparação do trabalho da CPU (todas ligadas por padrão; o valor
0 volta ao comportamento de antes, no processo que chama o launcher):

| Variável | Efeito |
| --- | --- |
| VYCE_LATE_SIGNAL=0 | Cada sinal de fila e cada fila parada numa espera esperam a GPU terminar tudo (hoje o sinal vai para trás do que a GPU roda e uma thread própria encerra as submissões quando a GPU termina) |
| VYCE_FREE_PRESENT=0 | O present espera a GPU terminar tudo antes de copiar o quadro |
| VYCE_LATE_TICKS=0 | Uma espera de quadro cujo tick já passou dorme até o próximo |
| VYCE_ONE_ECL=0 | Cada lista vai à fila numa chamada própria (hoje uma chamada por submissão, no máximo VYCE_ECL_LISTS listas, 32) |
| VYCE_KEPT_POOL=0 | Threads novas a cada trabalho dividido (hoje 5 threads mantidas) |
| VYCE_ONE_PASS=0 | Cópia de referência e comparação a cada faixa declarada (hoje uma passada por submissão) |
| VYCE_PLAN_EXTRA=0 | O plano de descritores inteiro é refeito por poucos slots novos |
| VYCE_OPENING=0, VYCE_PIN_ALWAYS=0 | As cópias próprias para a memória do jogo voltam a fixar páginas; o caminho rápido de guarda volta a ignorar as fixações |
| VYCE_WIDE_REFRESH=0 | As atualizações das cópias de um grupo rodam uma após a outra |
| VYCE_ASK_AHEAD=0 | Cada atualização pergunta ao kernel pelas páginas escritas da sua faixa (hoje uma pergunta por grupo) |
| VYCE_JOIN_COPIES=0 | Cópias do jogo que continuam a anterior ficam em passos separados |
| VYCE_LAZY_PRELUDE=0 | Toda lista nasce com a sua lista de transições, use ou não |
| VYCE_SHARED_LOOKUPS=0 | As buscas de memória das gravações tomam a tabela só para si |
| VICTORY_FREE_TABLES_AT_LANDING=0 | Os descritores dinâmicos das tabelas de uma lista só voltam quando o jogo reinicia ou solta a lista, sem empréstimo, espera nem nova tentativa quando faltam (hoje voltam quando a GPU termina a última submissão da lista, para as listas que o jogo usa uma vez por gravação) |
| VICTORY_TABLE_RETRY_MS=0 | Uma tabela sem descritores dinâmicos livres é descartada na hora (hoje espera até 100 ms a submissão mais antiga em voo terminar e tenta de novo uma vez; outro número muda essa espera, no máximo 2000) |
| VICTORY_SLOTS_ACROSS_CURSOR=0 | A busca de descritores dinâmicos livres para no ponto onde começou, como antes, e não acha uma faixa livre que passa por esse ponto; aí também anota que não há faixa daquele tamanho, e tabelas desse tamanho ou maiores deixam de ser feitas até algum descritor voltar (hoje a busca segue até n - 1 descritores depois desse ponto, nunca além do último, nas duas passadas: a dos livres não emprestados e a de quaisquer livres) |
| VICTORY_WALKS_PER_HEAP=0 | O limite das buscas de descritores dinâmicos que não acham faixa livre (8 por quadro; depois delas, tabelas do tamanho que falhou ou maiores não são feitas até o fim do quadro) volta a contar todos os heaps juntos: as falhas do heap de vistas recusavam as tabelas do heap de samplers, e vice-versa (hoje cada heap tem o seu limite) |
| VICTORY_NULL_COPY=0 | Uma cópia pedida pelo jogo à memcpy do VCRUNTIME140 a partir da página nula volta a encerrar o processo (hoje, só nesse caso estreito, o destino recebe zeros e a chamada volta; é a queda da missão do barco, aviso "a copy from the null page read as zeros") |
| VYCE_TAIL_DRAIN=0 | Cada submissão do jogo volta a esperar a anterior terminar antes de ser preparada, exatamente como antes (hoje é preparada com a anterior ainda na GPU e espera só antes de entregar as listas; ver Fase 2, abaixo). Para o 5c99db2 exato, junto com VYCE_RETAKE_GAPS=0 |
| VYCE_RETAKE_GAPS=0 | A cópia de referência de uma cópia não esparsa guarda, para uma faixa declarada no vão entre duas anteriores, os bytes que tomou como enchimento quando cresceu, como antes (hoje a declaração retoma esses bytes da cópia; sem isso uma escrita da GPU podia se perder, ver Fase 2) |

Campos de VYCE_TELEMETRY desse trabalho (por quadro): gpu_idle_ms (GPU
parada entre submissões), wait_*_ms e drains_* (tempo e número de esperas
pela GPU por motivo: execute, signal, work, held, present, fence, list,
e da fase 2 tail e flush),
late_signals, late_waits, landed e landed_here (submissões encerradas, e
as encerradas por quem precisou antes da thread própria), ecl_calls,
host_lists, watch_asks, watch_ms e watch_ahead (perguntas ao kernel e
respostas do grupo), copies_joined, flights_max, slots_max (descritores
dinâmicos ocupados no máximo), slots_freed_at_landing (devolvidos quando a
submissão da lista terminou), table_retries e table_retry_ms (tabelas sem
descritores livres que esperaram uma submissão terminar, e o tempo dessa
espera), tables_lost (envios de listas reenviadas que rodaram sem os seus
comandos), lent_taken (descritores emprestados que outra tabela tomou por
falta de outros livres).

### O que veio do Belobug (belobug/main até 4f62c2a)

A linha antiga do Belobug, a mesma memória no lugar do Vyce, entrou por
merge. Cada mudança tem a sua chave para voltar ao de antes (no processo
que chama o launcher):

| Variável | Efeito |
| --- | --- |
| VICTORY_AMBIENT_SKY=off | A luz ambiente volta a ser a constante de VICTORY_AMBIENT. Por padrão ela segue o céu que o jogo desenha para os reflexos: nível gain * média^power + least, com a cor do céu, de dia e de noite; gain,least,power muda os três (0.36,0.05,0.4) |
| VICTORY_AMBIENT=nível | Com o céu seguido, escala essa luz: 0.6, o padrão, deixa como o céu dá, 1.2 dobra, 0 desliga. Sem o céu, é a constante de antes |
| VICTORY_PROBE_RAYS=1 | Roda o programa que escolhe os raios das sondas de tela (CS_ScreenProbes_ActivateRays), deixado de fora enquanto não há raios: rodando sem raios atrás, a imagem toda perdeu luz, sombras e reflexos |
| VICTORY_PLAN_WRITES=1 | Um slot que só escreve outra faixa volta a contar como algo que falta ao plano de descritores; com VYCE_PLAN_EXTRA=0 o plano inteiro volta a ser refeito por ele |
| VICTORY_LOD_CLAMP=0 | A vista de uma textura posicionada com nível mínimo de detalhe volta a montar a textura com todos os mips |
| VICTORY_COMPILE_HOLD=20,150,3,0 | O vigia das compilações sem fim volta aos limites de antes. Hoje 60,300,8,2: segundos até suspeitar, segundos até parar de qualquer jeito, GiB que o processo cresce, e GiB livres da máquina (os 32 GB daqui) abaixo dos quais para logo uma compilação com mais de 20 s, se o processo cresceu 1 GiB ou mais desde que ela começou. Essa fica de fora só nesta sessão e nunca vai para a lista dos deixados de fora: o jogo, no mesmo processo, também cresce gigabytes carregando o mundo. Se o processo cresceu menos, a falta de memória não é dela: o log avisa e ela segue. Só o tempo (300 s) e o crescimento (8 GiB depois de 60 s) põem um pipeline na lista |
| VICTORY_SAVE_STRICT=1 | Uma versão de save que não pode ser apagada volta a impedir a leitura do container (o jogo então começava sem as configurações) |
| VICTORY_SHADER_OPT=all | Liga as passadas de limpeza dos shaders traduzidos (desligadas aqui: na RTX 2070 SUPER davam 0,2 % de código a menos por 2,3 vezes o tempo de tradução). Desligadas, o cache de shaders é o mesmo de antes delas; ligadas, a primeira sessão traduz tudo de novo. Um número escolhe as passadas (1, 2, 4, 8, 16), e VICTORY_SHADER_OPT_TRACE=1 mostra o que a de código morto tira |

Na compilação diag, VICTORY_SHADER_DUMP também salva os shaders dos
pipelines que o driver recusa, e a cada 30 s o log conta as texturas
posicionadas (quantas, quanto da memória do jogo cobrem, o que o pool e o
host guardam).

Para abrir o painel de uma sessão já em execução sem reiniciar o jogo,
execute scripts\vyce-monitor.ps1 e passe -GamePid, -StdoutLog e -StderrLog.

## Fase 1 (build A)

A build A é a vyce/2070s (antes em ea4e486) com as duas partes prontas da
fase 1, juntadas por merge nesta ordem, e uma correção pequena:

1. vyce/wt-upstream (16afc3d): a linha antiga do Belobug (luz ambiente que
   segue o céu, plano de descritores que uma escrita sozinha não refaz,
   saves mais tolerantes, mips das texturas posicionadas, o programa dos
   raios das sondas de tela de fora), o tamanho da imagem e a escala da
   cena no código do jogo, a checagem do pool de back buffers pulada em
   qualquer tamanho e o vigia das compilações com piso de memória, que
   nunca põe um pipeline bom na lista dos travados.
2. vyce/wt-slots (ace4e94): os descritores dinâmicos das tabelas voltam
   quando a submissão da lista termina, os devolvidos ficam emprestados e
   são tomados por último, uma lista que perdeu os seus roda inteira sem os
   comandos até o jogo reiniciá-la, uma espera curta quando faltam e a
   proteção da queda da missão do barco.
3. A busca de descritores dinâmicos livres acha uma faixa livre que passa
   pelo ponto onde a busca começa. Antes ela não achava essa faixa, nem
   entre os livres não emprestados nem entre quaisquer livres, e ainda
   anotava que não havia faixa daquele tamanho: tabelas desse tamanho ou
   maiores deixavam de ser feitas até algum descritor voltar.

A correção de shaders (vyce/wt-shader) ainda não entrou. O tradutor é o
mesmo: a impressão dele continua 5d536b05afbcb18a491c46fd06fd99a3 (as
passadas de limpeza ficam desligadas), então o cache de shaders já
aquecido vale para a build A e a primeira sessão não traduz nada de novo.

Para jogar, em PowerShell na raiz do Vyce:

~~~powershell
.\scripts\vyce.ps1 -Action Run
~~~

Sem opções, o launcher põe VICTORY_OUTPUT=1920x1080 (o código do jogo
monta a imagem em 1920x1080 e a cena em 1280x720) e escreve 1920x1080 no
settings.xml do jogo. Na build intermediária (só a vyce/wt-upstream, log
2026-11-27_00-19-39) isso deu GPU ocupada 141 ms por quadro na mediana
(antes 233), GPU parada 131 ms, 3,7 FPS na mediana e 2.045 acertos de 2.050
no cache de shaders. As mudanças da vyce/wt-slots e a correção da busca
ainda não foram medidas em jogo.

Cada mudança tem a sua chave para voltar ao de antes. As variáveis valem
no processo que chama o launcher, por exemplo:

~~~powershell
$env:VICTORY_FREE_TABLES_AT_LANDING = '0'
.\scripts\vyce.ps1 -Action Run
~~~

| O que mudou | Para voltar ao de antes |
| --- | --- |
| Tamanho da imagem 1920x1080 no código do jogo em vez de 3840x2160 (cena 1280x720), com cada lugar comparado byte a byte antes e tudo ou nada; o pulo da checagem do pool de back buffers vem junto | -NoTitlePatch (só o settings.xml muda), -OutputSize keep (nem o arquivo nem o código) ou VICTORY_OUTPUT=off |
| Escala da cena no upscaler do jogo | Só com -RenderScale 0.5 a 1.0 (VICTORY_RENDER_SCALE); sem ela ficam os 2/3 do jogo |
| Upscaler do próprio jogo no Series S (o launcher põe VICTORY_UPSCALER=6 com -ConsoleProfile series-s) | -NoTitlePatch ou VICTORY_UPSCALER=off |
| Luz ambiente que segue o céu dos reflexos | VICTORY_AMBIENT_SKY=off (VICTORY_AMBIENT=nível escala qualquer das duas luzes) |
| CS_ScreenProbes_ActivateRays deixado de fora enquanto não há raios | VICTORY_PROBE_RAYS=1 |
| Um slot que só escreve outra faixa não refaz o plano de descritores | VICTORY_PLAN_WRITES=1 |
| A vista de uma textura posicionada com nível mínimo de detalhe vai sem os mips abaixo dele | VICTORY_LOD_CLAMP=0 |
| Uma versão de save que não pode ser apagada não impede a leitura do container, e a atualização seguinte vai para um v<N>.<k>.new livre | VICTORY_SAVE_STRICT=1 |
| Vigia das compilações: 60 s e 8 GiB de crescimento, ou 300 s, põem o pipeline na lista; com menos de 2 GiB livres na máquina, uma compilação de mais de 20 s que fez o processo crescer 1 GiB para só nesta sessão | VICTORY_COMPILE_HOLD=20,150,3,0 |
| Passadas de limpeza dos shaders traduzidos: vieram desligadas | VICTORY_SHADER_OPT=all liga (o cache muda e a primeira sessão traduz tudo de novo) |
| Build diag: shaders dos pipelines recusados salvos e contagem das texturas posicionadas a cada 30 s | Só na build diag, os shaders com VICTORY_SHADER_DUMP=pasta |
| Os descritores dinâmicos de uma lista voltam quando a GPU termina a última submissão dela, emprestados enquanto o jogo pode enviá-la de novo; uma lista reenviada que perdeu os seus roda sem os comandos até reiniciar | VICTORY_FREE_TABLES_AT_LANDING=0 |
| Uma tabela sem descritores livres espera até 100 ms a submissão mais antiga em voo terminar e tenta de novo uma vez (no máximo 1 s de espera por quadro) | VICTORY_TABLE_RETRY_MS=0 |
| A busca de descritores dinâmicos livres segue até n - 1 descritores depois do ponto onde começou, nunca além do último | VICTORY_SLOTS_ACROSS_CURSOR=0 |
| Uma cópia da página nula pedida à memcpy do VCRUNTIME140 pelo código do jogo (a queda da missão do barco) dá zeros no destino | VICTORY_NULL_COPY=0 |

No launcher, -OutputSize agora também muda o código do jogo, e há duas
opções novas, -RenderScale e -NoTitlePatch (em Opções do jogo, acima).
Campos novos de VYCE_TELEMETRY: slots_freed_at_landing, table_retries,
table_retry_ms, tables_lost e lent_taken (em Painel de diagnóstico, acima).

Testada com todas as suítes pelo heavy.sh: victory-gpu na placa com 194
testes passando e 9 ignorados (também com VICTORY_FREE_TABLES_AT_LANDING=0
e VICTORY_SLOTS_ACROSS_CURSOR=0), victory-shader com 293 passando e as 4
valu_tests que já falhavam na NVIDIA, victory-hle com 99 passando e os 5
testes de xmem que já falhavam, victory-loader com todos passando menos um
do load_probe (falta a delayimp.lib no toolchain do laboratório) e os quatro
lugares do código conferidos no executável 1.0.2.1, victory-core e o app.

## Fase 1 (build B)

A build B é a build A (vyce/2070s 2081287) com a correção de shaders
juntada por merge, sem conflitos, e uma correção do comando warm:

1. vyce/wt-shader (171e532): códigos normalizados (UNORM/SNORM) de buffers
   exatos, dados de 16 bits de vistas de formato inteiro sem passar por f16,
   programas wave64 recusados tentados de novo sem as saídas antecipadas (a
   de dentro de um laço fica), os modificadores de v_interp_p2_f16, a
   tabela de passos lida uma vez por vista e bloco, e os grupos de uma onda
   com memória compartilhada rodando como duas ondas locais de 32 lanes que
   só se encontram onde as duas rodam o mesmo número de vezes (as correções
   dos travamentos da GPU e a análise do que as duas ondas podem ter
   diferente). As chaves do tradutor ficam fora das regiões das passadas de
   limpeza (as marcas `// clean-up passes: begin/end` continuam como
   estavam).
2. O warm não esvazia mais a lista dos pipelines do jogo que começam cedo
   no carregamento (cache\pipelines\early-pipelines.txt): ao salvar a
   biblioteca de pipelines ele gravava a lista que tinha lido, e ele não
   lê nenhuma. Agora a lista é lida antes de ser gravada. No jogo nada
   muda: ele a lê no primeiro pipeline do jogo, antes de qualquer gravação.
   Sem chave: o comportamento antigo só perdia a lista. O resumo do warm
   diz agora o que veio dos caches.

O tradutor mudou: a impressão dele passou de
5d536b05afbcb18a491c46fd06fd99a3 para d291d9a04367aead5a126f9532c6da45, e
a primeira sessão traduziria todos os shaders e montaria os pipelines de
novo no disco (na última vez, cerca de 1.700 pipelines e 11,5 minutos até
o mundo). Por isso os caches foram preparados antes, com o jogo fechado, com
a build release feita pelo launcher (scripts\vyce.ps1 -Action Build) e o
ambiente que o launcher dá ao jogo:

~~~powershell
victory warm D:\torrent\Vyce-data\lab\shaders\host --pipelines <lista> --jobs 8
~~~

A lista tem os 954 programas de compute de lab\compute-used.txt e os de
todas as sessões em Vyce-data\logs (960 ao todo). Em 253 s: 11.199
contêineres traduzidos para o cache de shaders (todos novos), 72 recusados
(endereço num SGPR, wave64 sem forma de 32 lanes, saídas de malha), 1.648
pulados por serem DXIL de PC, sem programa do console; 631 pipelines de
compute na biblioteca (534 já estavam lá, de shaders cujo DXIL não mudou,
e 97 compilados), nenhum recusado pela placa, 39 programas que não
traduzem e 290 deixados de fora pelas regras do jogo (247 resolves de
raios, 42 da lista do cabelo e 1 dos raios das sondas). Os 626 pipelines
de compute da última sessão estão entre eles. O alvo veio da placa, como
no jogo: o log diz views=1000000 e lanes_min=32 lanes_max=32
exact_views=true. Um segundo warm, em outro processo, achou no cache os 50
contêineres que testou. A lista dos que começam cedo ficou com as mesmas
2.042 entradas.

O que ainda fica para a primeira sessão: 177 dos 2.050 shaders da última
sessão não estão em lab\shaders\host (são todos de desenho: 89 de pixel,
81 de malha e 7 de vértice) e são traduzidos em jogo, e os pipelines de
desenho só podem ser montados com o estado que o jogo passa. Pelos
pipelines que a última sessão usou, no máximo perto de 280 deles mudam
(202 de malha com um shader mudado e 54 com um shader fora da pasta; 22 de
vértice e pixel) e são compilados em jogo, começando no carregamento pela
lista; os outros (408 dos 686 de desenho) vêm da biblioteca. Uma chave do
tradutor trocada é outro cache: a primeira sessão com ela traduz tudo de
novo.

Para jogar, em PowerShell na raiz do Vyce:

~~~powershell
.\scripts\vyce.ps1 -Action Run
~~~

A build A ficou ao lado da nova, como
Vyce-data\target\release\victory-2081287.exe; o cache dela continua no
mesmo lugar, nada foi apagado.

| O que mudou | Para voltar ao de antes |
| --- | --- |
| Códigos normalizados de buffers com o valor exato, não a divisão da GPU (126 dos 256 códigos de 8 bits erravam o último bit nesta placa) | VICTORY_EXACT_NORM=0 |
| Dados de 16 bits de vistas de formato inteiro com os bits como estão, marcados na tabela de passos | VICTORY_D16_INTEGER=0 |
| Programas wave64 recusados tentados de novo sem as saídas antecipadas e os saltos que o código depois deles não vê | VICTORY_WAVE_FORMS=0 |
| A saída antecipada de dentro de um laço fica (pode ser a única saída dele) | VICTORY_LOOP_EARLY_OUTS=0, só para comparar: pode travar a GPU |
| v_interp_p2_f16 escreve a metade que op_sel pede, depois do multiplicador e da saturação | VICTORY_INTERP_F16_MODS=0 |
| Resultados de 16 bits arredondados ao mais próximo, como no console | Desligado por padrão; VICTORY_F16_NEAREST=1 liga |
| A tabela de passos lida uma vez por vista e bloco | VICTORY_STRIDE_ONCE=0 |
| Grupos de uma onda com memória compartilhada como duas ondas locais de 32 | VICTORY_LDS_LOCAL=0 |
| As duas ondas locais só se encontram onde as duas rodam o mesmo número de vezes | VICTORY_LOCAL_MEET_CHECKS=0, só para comparar: pode travar a GPU |
| A análise do que as duas ondas podem ter diferente conta acumuladores, máscaras de seleção e leituras de memória que o dispatch escreve | VICTORY_HOST_WAVE_READS=0, só para comparar: pode travar a GPU |
| O warm grava de volta a lista dos pipelines que começam cedo | Sem chave (antes ele a esvaziava) |

Testada pelo heavy.sh: victory-shader com 310 passando e as 4 valu_tests
que já falhavam na NVIDIA (com VICTORY_DXIL, as 26 split_tests passam),
victory-gpu na placa com 196 passando e 9 ignorados, victory-hle com 99
passando e os 5 testes de xmem que já falhavam, victory-loader com todos
passando menos um do load_probe (falta a delayimp.lib no toolchain do
laboratório) e os lugares do código conferidos no executável 1.0.2.1,
victory-core e o app. Ainda não foi medida em jogo.

## Fase 2: pendências menores da fase 1

A branch vyce/wt-minors (de vyce/2070s 5c99db2) corrige as cinco
pendências pequenas que as revisões da fase 1 deixaram:

1. A compilação do launcher é offline (em Comandos, acima): -Action Build
   passava --locked, mas deixava o Cargo baixar. Agora o processo do Cargo
   recebe CARGO_NET_OFFLINE=true, seja o que for que o ambiente de quem
   chama diga (e o valor de antes volta ao sair); -Online deixa baixar.
2. A lista dos pipelines que começam cedo só conta como vazia quando o
   arquivo não existe (em Cache pronto antes de jogar, acima). Sem chave:
   o comportamento antigo só apagava a lista.
3. Uma leitura escalar simples de uma vista de acesso não ordenado ou sem
   tipo declarado conta como algo que as duas ondas locais podem ler
   diferente em qualquer programa, não só nos que escrevem na memória:
   outro dispatch pode escrever nessa vista enquanto este roda. Nenhum
   programa de lab\shaders\host muda com isso.
4. Um programa com um desvio que as duas ondas locais tomariam diferente
   só por uma leitura de lane de um valor que as lanes têm diferente vai
   antes para o fluxo do console, que dá o resultado do console (no teste
   LDS_FMAC, 64 como no console, onde as ondas locais davam 32). A regra
   ao pé da letra mudaria 12 programas da pasta, mas em todos a leitura
   era de um valor igual em toda lane: o índice do grupo tirado do índice
   da thread em 6 programas de 64 threads (dois deles na lista de compute
   que o jogo usou), um buffer lido num índice só em 5 e uma textura lida
   num ponto só em 1. A leitura da primeira lane de um valor assim não
   conta mais. Mudam 5 programas, nenhum deles nas listas de compute do
   jogo: os do buffer num índice só, que rodam na forma sem saídas
   antecipadas, onde essa leitura fica sob a condição de uma lane só (a
   segunda onda local não tem lane ativa ali e leria um valor qualquer).
5. O limite de 8 buscas de descritores dinâmicos que não acham faixa livre
   por quadro é de cada heap: antes um heap que gastava o limite recusava
   as tabelas do outro até o fim do quadro.

| O que mudou | Para voltar ao de antes |
| --- | --- |
| Compilação do launcher offline | -Online |
| Só a falta do arquivo conta como lista vazia de pipelines que começam cedo; com outro erro de leitura ela não é regravada, e o que a sessão usou espera o primeiro salvamento que a ler | Sem chave (antes um erro de leitura apagava a lista) |
| Leituras escalares simples de vistas que podem ser escritas contam como diferentes entre as duas ondas locais em qualquer programa | VICTORY_VIEW_LOADS_APART=0, só para comparar: pode travar a GPU |
| Um desvio que só uma leitura de lane separa vai para o fluxo do console | VICTORY_LANE_READ_BRANCHES=0 |
| O limite das buscas de descritores que não acham faixa livre é de cada heap | VICTORY_WALKS_PER_HEAP=0 |

O tradutor mudou com os itens 3 e 4: a impressão dele passou de
d291d9a04367aead5a126f9532c6da45 para dc0e869b0f4d6e0ca4a17f17abd1891f,
e a primeira sessão sem um warm traduz todos os shaders de novo.

Conferido pelo heavy.sh: o regx sobre lab\shaders\host (12.919 contêineres,
32 lanes, vistas exatas), contra a 5c99db2, deu 5 programas mudados (os do
item 4), nenhum com VICTORY_LANE_READ_BRANCHES=0 e nenhum com as duas
chaves novas do tradutor desligadas; o valx, 11.199 válidos, 1.720
recusados como antes e nenhum inválido; os 707 programas de compute usados
(lab\shaders\used-cs), 668 pipelines feitos na placa, nenhuma falha e os
39 que não traduzem. As suítes: victory-shader com 312 passando e as 4
valu_tests que já falhavam na NVIDIA (com VICTORY_DXIL, as 28 split_tests
passam, as de LDS_FMAC e do índice do grupo rodando na placa),
victory-gpu na placa com 198 passando e 9 ignorados (também com
VICTORY_WALKS_PER_HEAP=0) e o teste de integração do tradutor. A
compilação offline foi conferida com um cargo.exe de mentira num par
Vyce/Vyce-data de teste (8 casos: offline por padrão, -Online, o ambiente
de quem chama, o aviso quando falha), e a release compila. Ainda não foi
medida em jogo.

## Fase 2: texturas de volta (texback)

Algumas texturas que os shaders do jogo escrevem são lidas depois pelo
código do próprio jogo, na CPU. No console a CPU e a GPU usam a mesma
memória: o que um passe de compute grava numa textura fica na memória do
jogo. No Vyce a textura é uma cópia no host, e o que os shaders gravavam
ficava só nela. Os mapas de altura e de deslocamento da água são assim: o
jogo os lê na CPU para os barcos boiarem, e lia memória que ninguém tinha
escrito.

Agora, para as texturas dos tipos de VICTORY_TEXTURE_BACK (por padrão os
quatro da água: 256x256 R32_FLOAT, 256x256 R32G32_FLOAT, 64x64
R32G32_FLOAT e 128x128 R16G16_UINT), uma lista cujos shaders escrevem uma
delas por uma tabela de descritores copia a textura para um buffer de
leitura nosso duas vezes: logo antes do primeiro draw ou dispatch que pode
escrevê-la e quando a lista fecha. Quando a submissão termina, no pouso
(depois da cópia de volta dos buffers e das leituras de textura do jogo,
antes das escritas do fim do pipeline e das cercas que o jogo espera), os
elementos que mudaram entre as duas cópias vão para a memória do jogo, no
layout do console, e só onde a memória do jogo é diferente. Um elemento que
a CPU do jogo escreveu e os shaders da lista não mexeram fica com o valor
do jogo, e páginas onde nada mudou não são escritas. A cópia de upload da
textura (quando existe) não toma essa escrita como do jogo, então não manda
um quadro antigo por cima do que a GPU escreveu depois; as outras cópias da
mesma memória a veem escrita, como veriam a escrita da GPU do console.

Vem do github.com/avatarmanz/VIctory-7900XTX (23925a4, que traz o
fxceless/victory-fork f5c03f3b, PR 999 de Geckomacabre e
cxy0072019-sketch) e do codeberg.org/p1nkman/p1nkman-VI (9aaa9ec), os dois
GPL-3.0: os tipos, a variável e os tíquetes. As duas cópias, a comparação e
o pouso foram escritos para o modelo do Vyce.

Fica ligado por padrão. Os forks que já fazem isso rodam ligado: no
VIctory-7900XTX está ligado no código e o barco fica em cima a sessão
inteira (sem isso ele afundava em todas); o p1nkman-VI (RTX) e o
victory-3060TI deixam desligado no código, mas os launchers de NVIDIA de
8 GB deles ligam (VICTORY_TEXTURE_BACK=1), e eles contam o barco
boiando e a missão do barco jogável. O custo é pequeno: com as quatro
texturas escritas uma vez por quadro, cerca de 1,7 MB lidos da placa por
quadro e alguns décimos de ms de CPU no pouso. Quando nenhuma textura
desses tipos é escrita não custa nada (uma comparação por vista de escrita
de textura gravada).

Os blocos flutuando: a investigação da fase 2 (lab\fase2-desenho.json,
floating-blocks) mostrou que são props e pessoas inteiros, de pé,
levantados perto da água, e uma das famílias suspeitas é esta: o próprio
jogo os põe ali a partir do que lê de texturas que a GPU escreveu. Não está
provado que são estas texturas. Para conferir, no mesmo save e no mesmo
ponto da praia, uma sessão com VICTORY_TEXTURE_BACK=0 e outra sem a
variável: com a cópia de volta o barco deve boiar; se os objetos descem para
o chão, eram estas texturas.

| O que mudou | Para voltar ao de antes |
| --- | --- |
| Texturas dos quatro tipos da água que os shaders escrevem voltam para a memória do jogo quando a submissão termina, antes das cercas | VICTORY_TEXTURE_BACK=0 |
| Outros tipos podem ser pedidos: largura x altura e formato DXGI, separados por vírgula (1 volta aos quatro da água) | VICTORY_TEXTURE_BACK=256x256f41,64x64f16 |

Campos novos de VYCE_TELEMETRY: textures_back (texturas copiadas de volta
por quadro), textures_back_kb (KB escritos na memória do jogo por quadro) e
textures_back_read_kb (KB lidos da placa por quadro). copy_back_ms e
guest_write_mb já incluem essas texturas. O log de 5 em 5 s diz "of it N
textures X KB" depois de "copying back", e no painel a dica de "Cópia de
volta" mostra "Texturas copiadas de volta por quadro".

O que fica de fora: escritas por slots indexados direto no heap
(ResourceDescriptorHeap[]), por render target, cópias e resolves para a
textura, o que veio antes do primeiro draw ou dispatch da lista que pode
escrevê-la, e um elemento que os shaders reescrevem com o valor que ele já
tinha (a memória do jogo fica com o que tinha ali). Também ficam de fora
texturas sem os mips maiores no host, em layouts com XOR ou acima de 16 MiB.
Uma leitura do jogo entre a submissão e a cerca vê o que havia antes, como
no console antes da GPU rodar.

Com o dreno no fim (a etapa B dos voos, abaixo): Flight.textures traz, para
cada textura, Back::range(), a faixa da memória do jogo que o pouso pode
escrever, e ela entra nas marcas do voo (as faixas que o pouso escreve);
uma atualização dessa memória no preparo seguinte espera o pouso (build C).
Cada gravação tem um só buffer de leitura: com mais de um voo no ar (passo 3
da etapa B) uma segunda submissão da mesma gravação teria de esperar a
primeira pousar.

Testada pelo heavy.sh: victory-gpu na placa com 204 passando e 9 ignorados
(os 196 de antes e 8 novos: a textura que um shader escreve está na memória
do jogo, no layout do console e byte a byte, antes de alguém ver a cerca,
no WARP e na placa, e um elemento que o jogo escreveu e o shader não mexeu
fica com o valor do jogo; só os tipos da lista ganham cópias; uma lista que
não roda e uma textura liberada não escrevem nada; a cópia de upload não
toma a nossa escrita como do jogo; a leitura da variável, os tíquetes e a
comparação), também com VICTORY_TEXTURE_BACK=0; victory-hle com 99
passando e os 5 testes de xmem que já falhavam; a build release passa.
Ainda não foi medida em jogo.

## Fase 2: programas wave64 em pares de lanes (vyce/wt-pairs)

Os quatro programas de compute wave64 que nenhuma forma de 32 lanes
aceitava agora traduzem exatos e montam pipeline na RTX 2070 SUPER:

| Programa | O que faz | Antes |
| --- | --- | --- |
| CS_ShadowUnifiedClearRaster (0c028454b58543f0) | Cabeça da cadeia das sombras em blocos: zera contadores e imagens e prepara os registros que os resolves das sombras leem | Descartado a cada quadro; o suspeito mais forte das sombras escuras em blocos e de parte da imagem escura |
| CS_DefragHeap_ComputeRelocations (02384a60fda6f2d0) | Planeja a compactação do heap de índices das partículas (VFX) | Descartado a cada quadro: o heap nunca era compactado |
| CS_SortRayTiles (a1bf269d5b19f025) | Ordena os tiles de raios | Descartado a cada quadro: tiles fora de ordem |
| CS_AssembleComputeList (b0d5ffa0801b7450) | Monta uma lista de tiles de 16x16 | Nunca ligado nas sessões registradas |

Como funciona (a forma em pares, `emit::paired`): cada onda de 64 lanes do
console roda numa onda de 32 da placa, e cada thread faz o trabalho de duas
lanes do console, a n e a n + 32. O programa roda como no console: cada
desvio é tomado pela onda inteira e o EXEC guarda cada instrução de lane,
então as ondas de um grupo podem seguir caminhos diferentes e se encontram
nas barreiras do próprio programa. Máscaras lidas inteiras, leituras de
outras lanes e as reduções e somas da onda leem as duas lanes de cada
thread da onda, sem troca pela memória compartilhada. O grupo tem 32
threads por onda do console; a única barreira a mais é a da entrada, onde
cada onda da placa pega a próxima onda do console de um contador. Vem do
fxceless/victory-fork#854 (PiloteCode; victory-3060TI 26600c6 e p1nkman-VI;
GPL-3.0), adaptado ao fluxo do console do Vyce.

É a última tentativa do tradutor: só entra num programa wave64 de compute
que todas as outras formas recusam. Nos 12.919 contêineres do laboratório
nenhum programa que já traduzia mudou; além dos quatro, nove do mesmo tipo
que nunca foram ligados nas sessões registradas passam a traduzir também
(outra variante do DefragHeap, CS_ShadowUnifiedClearRT, quatro
CS_Compact*Classification, dois CS_BuildProjDecalTrail* e
CS_ScreenProbes_TraceRays_Fallback). A versão 0 recusa, e o programa fica
descartado como antes: grupos que não enchem as ondas do console, memória
por endereço, memória privada, DPP, permutações, escritas de lane e
derivadas fora das seções da onda, e somas de float da onda (o console
soma na ordem dele).

Duas suposições, como já nas outras formas de uma onda com memória
compartilhada: as threads de uma onda da placa rodam juntas quando seguem
o mesmo caminho (as duas lanes de uma onda do console se passam valores
pela memória compartilhada sem barreira, com uma cerca entre os acessos);
e uma onda parada pelo limite de voltas de laço sai do programa sem
segurar as outras numa barreira (a NVIDIA conta só as threads que não
terminaram). O D3D12 não promete nenhuma das duas.

O tradutor mudou: a primeira sessão traduz tudo de novo. Rode o warm antes
de jogar (ver "Cache pronto antes de jogar"), com as chaves que o jogo vai
usar: VICTORY_WAVE_PAIRS=0 é outro cache.

| O que mudou | Para voltar ao de antes |
| --- | --- |
| Programas wave64 de compute que nenhuma outra forma aceita rodam em pares de lanes (uma onda de 32 da placa por onda do console, duas lanes do console por thread) | VICTORY_WAVE_PAIRS=0 |

Testada pelo heavy.sh na RTX 2070 SUPER: victory-shader com 320 passando e
as 4 valu_tests que já falhavam na NVIDIA. Os 10 testes novos
(`emit::paired_tests`, também com o validador em VICTORY_DXIL) rodam na
placa programas montados à mão nos padrões
dos quatro, cada um contra um modelo na CPU do que as ondas do console
calculam: duas ondas em laços de tamanhos diferentes com a lane 63 passando
um valor pela memória compartilhada sem barreira (5 e 9 voltas, e 1.000 e
1.001), o "algum" de cada onda em volta de mínimo e máximo da onda com o
slot da primeira lane, uma ordenação por contagem com laços enquanto
alguma lane tem registros, somas prefixadas e os totais da lane 63, e as
contagens "algum e algum" em duas rodadas anexadas a uma lista; os casos
do fluxo do console em grupos de uma, duas e quatro ondas; e a primeira
lane ativa vinda da metade de cima, ou a lane 0 quando nenhuma está. Um
ballot, uma
leitura de lane ou um total errados na metade de cima fazem esses testes
falharem (conferido). Nos 12.919 contêineres do laboratório (32 lanes,
vistas exatas): nenhuma tradução mudou, 13 passaram a traduzir (os quatro e
os nove acima); o validador da Microsoft deu 11.212 válidos, 1.707
recusados e nenhum inválido; o conjunto de compute usado deu 672 pipelines
montados na placa, nenhuma falha e os mesmos 35 recusados (endereço num
SGPR), os quatro entre os montados; os nove também montam (o maior, com 320
KB de DXIL, em 3,5 s). A build release passa. Ainda não foi medida em
jogo.

## Fase 2: dreno no fim (etapa B dos voos, passo 1)

Na sessão da build intermediária a GPU ficou parada 131 ms por quadro (mediana)
porque cada submissão do jogo esperava a anterior terminar antes de ser
preparada (`execute_inner` chamava `line.drain` logo no começo): os ~44 ms de
preparo de cada quadro, em ~23 submissões, aconteciam com a placa sem trabalho.
Agora a submissão é preparada com a anterior ainda na GPU, e a espera vai para
o fim do preparo, logo antes de `keep_before` e de `ExecuteCommandLists`. No
máximo uma submissão do jogo fica no ar durante um preparo, e ela pousa antes
da entrega das listas da próxima: os limites de descritores, cópias de
referência, pares de tempo, a lista Early e as listas do card ficam como eram.
É o passo 1 do plano do juiz da fase 2 (lab\fase2-desenho.json, chave
"judge:stage-b"); o código está em `crates/victory-gpu/src/tail.rs`.

Cada passo do preparo é de um de três tipos:

- feito na hora, como antes: atualização de cópias de memória que nenhum
  pouso no ar escreve, faixas entregues aos shaders, os slots de descritores
  que o jogo escreveu desde o plano anterior (o jogo só reescreve slots que
  nada em voo lê, como no console);
- deixado para depois da espera, na ordem em que vinha: atualização de cópias
  de memória que o pouso no ar escreve (o que os shaders dele escreveram nas
  cópias, as leituras de volta, as texturas copiadas de volta inteiras, as
  escritas de fim de lista e o que outras
  filas puseram atrás dele), de memória usada no lugar pela GPU (as escritas
  dela ali não são conhecidas) ou do que os shaders dele escrevem pelos slots
  de acesso desordenado das pilhas indexadas direto (isso fica na cópia, não
  volta), ali ou em outra vista das mesmas páginas (abaixo); e todo passo que
  lê a memória do jogo para memória nossa ou escreve o que o voo ainda pode
  ler: preenchimentos, o trabalho do runtime, texturas e os seus envios,
  prelúdios, os rebotes de constantes e as cópias de referência de cópias que
  não são esparsas;
- uma espera onde está (ponto de dreno): cópias de buffers do jogo
  (`CopyBufferRegion`, `CopyResource`), `CopyMemoryToMemoryX`, a declaração
  de escrita de shaders numa cópia não esparsa que o voo no ar também
  escreve, e o plano de uma pilha indexada direto quando ele vai fazer slots
  que o jogo não escreveu desde o plano anterior: os que esse plano deixou
  por fazer (estourou o tempo dele), que o jogo escreveu para trabalho que
  pode estar no ar, e os que o olhar de uma vez por quadro acha (vistas
  soltas, texturas despejadas, janelas de memória mapeada de novo, memória
  antes sem mapa): o voo pode estar lendo esses slots, e um descritor não pode
  mudar enquanto a GPU pode lê-lo (lido metade velho, metade novo, um
  endereço errado derruba o dispositivo). O plano espera ali, sem trava
  nenhuma, antes de fazer o primeiro slot. Dali em diante o preparo é o de
  antes.

O pouso escreve por endereço, e o jogo mostra a mesma página física em vários
endereços (vistas das seções do xmem). Uma atualização dos mesmos bytes por
outra vista, de página cujas vistas são guardadas juntas (de 2 a 64 vistas),
espera o pouso também, e o mesmo vale para outra vista de memória usada no
lugar (`watch::other_views`, perguntado na hora de cada atualização, com as
vistas como estão então). O dreno antigo via essas escritas pelas guardas:
as nossas escritas na memória do jogo abrem todas as vistas da página, e a
atualização vinha depois delas.

Uma atualização deixada para depois contorna as faixas que a submissão já
tinha entregue aos shaders no passo dela, e só essas: depois do dreno antigo
eram as únicas fechadas. As faixas que os shaders escrevem levam agora o
valor de fence da submissão, e um pouso abre só as suas (antes eram tiradas
por valor, e o pouso da anterior abria a mesma faixa declarada pela
seguinte). As cópias de referência de blocos de uma cópia esparsa voltam ao
pool por voo, as que nada mais pendente toca (antes só quando nada estava
pendente; com o preparo sob o voo isso nunca aconteceria). A pré-atualização
(prewarm) fica de fora com o dreno no fim: o próprio preparo já corre sob o
voo, e ela eram as mesmas atualizações feitas duas vezes, ~6 ms por quadro na
thread do jogo que submete (VYCE_TAIL_PREWARM=1 a mantém).

Achado pelo teste com a pré-atualização de fora, e que já existia no dreno
antigo sempre que ela não rodava (nenhum voo no ar): a cópia de referência
de uma cópia não esparsa cresce para cobrir todas as faixas que a submissão
declara, e o vão entre duas faixas era tirado da cópia como enchimento. Uma
atualização depois mudava a cópia no vão, não a referência; uma terceira
faixa declarada ali ficava com a referência velha, e na volta uma escrita da
GPU de um byte igual ao enchimento se perdia (e uma escrita do jogo feita
enquanto a GPU rodava podia ser coberta pelo byte atualizado, mais velho).
Agora cada declaração retoma na referência os bytes dela que nenhuma faixa
anterior tomou (`Shadow::keep_dense`). Vale com os dois drenos;
VYCE_RETAKE_GAPS=0 volta ao de antes.

Ficam no dreno antigo, contados: submissões com uma lista ainda no ar
(reenviada pelo jogo) e as com shaders que escrevem buffers na VRAM (essas
esperam a GPU de qualquer jeito). O dreno no fim desliga sozinho, com uma
linha no log, com VICTORY_ASYNC_SUBMIT=0, VYCE_LATE_SIGNAL=0, VYCE_ONE_ECL=0,
VYCE_ONE_PASS=0, VICTORY_SWEEP_TILES diferente de 0 e, na build diag,
VICTORY_GPU_TIMING.

| Variável | Efeito |
| --- | --- |
| VYCE_TAIL_DRAIN=0 | Volta ao dreno antigo, exatamente: cada submissão espera tudo em voo antes de ser preparada (com VYCE_RETAKE_GAPS=0 também, o 5c99db2 exato) |
| VYCE_TAIL_DRAIN_STRICT=1 | Modo paranoico: toda atualização de cópia espera o fim, não só as que o pouso escreve. Se uma imagem errada com o dreno no fim fica certa com este modo, faltou alguma escrita na lista do pouso, da memória usada no lugar ou de uma vista |
| VYCE_TAIL_DRAIN_DRY=0 | Com o dreno antigo e a telemetria ligada, não conta o que o dreno no fim (o normal, não o estrito) faria (os campos b_dry_* abaixo) |
| VYCE_TAIL_PREWARM=1 | Com o dreno no fim, a pré-atualização roda antes do preparo, como antes (hoje fica de fora) |
| VYCE_RETAKE_GAPS=0 | A referência de uma faixa declarada num vão fica com o enchimento, como antes (vale para os dois drenos) |

O que não está coberto e continua como sempre esteve (só a ordem no tempo
pode mudar): escritas de shaders em cópias que ninguém declara (shaders que
escrevem por ponteiro; slots de acesso desordenado maiores que 8 MiB, ou
feitos desde que o plano de descritores foi montado inteiro pela última
vez) correm com a atualização da submissão seguinte quando o jogo escreve a
mesma página de 64 KiB; com o dreno antigo essas escritas sumiam sempre, agora
podem ficar. VYCE_TAIL_DRAIN_STRICT=1 devolve a ordem antiga para isso também.
Slots de descritores que o jogo reescreve enquanto um voo ainda os lê passam a
valer para esse voo (no console também valeriam: seria uma corrida do próprio
jogo); com o dreno antigo, não. Os que o plano anterior deixou por fazer e
os que o olhar acha esperam o voo (acima). Páginas mostradas em mais de 64 endereços (a página
"vazia" do jogo tem milhares) têm as vistas guardadas uma a uma, e as escritas
da GPU em memória usada no lugar só aparecem nas cópias de outra vista nos 2 s
em que o bloco fica fixado: nos dois casos uma escrita por uma vista não
chega às cópias das outras, com nenhum dos dois drenos, como antes.

Campos novos de VYCE_TELEMETRY, por quadro:

| Campo | Significado |
| --- | --- |
| b_mode | 0 dreno antigo, 1 dreno no fim, 2 estrito (o maior da amostra) |
| b_prepared_under | Submissões do jogo preparadas com outra no ar |
| b_overlap_ms | Preparo feito antes da primeira espera: o tempo que a GPU deixou de ficar parada (no máximo) |
| wait_tail_ms, drains_tail | Espera no fim do preparo (a que sobra) e quantas esperaram de fato |
| wait_flush_ms, drains_flush | O mesmo nos pontos de dreno no meio do preparo |
| b_flush_copy, b_flush_run, b_flush_dense, b_flush_slots | Pontos de dreno por motivo: cópia do jogo, CopyMemoryToMemoryX, cópia não esparsa que o voo também escreve, slots que o plano anterior deixou por fazer |
| b_after_flush_ms | Preparo depois da primeira espera (volta a ser tempo de GPU parada) |
| b_late_parts, b_late_mb | Atualizações de cópias deixadas para depois da espera, e os seus bytes |
| b_late_views | Delas, as que esperam por outra vista das mesmas páginas (o pouso escreve ali, ou ali é memória usada no lugar) |
| b_queued, b_queued_ms | Outros passos deixados para depois, e o tempo deles |
| b_old_list, b_old_card | Submissões que ficaram no dreno antigo: lista ainda no ar, shaders que escrevem buffers na VRAM |
| b_dry_prepared | (Dreno antigo) submissões que teriam sido preparadas sob o voo |
| b_dry_flush_copy, b_dry_flush_run, b_dry_flush_dense, b_dry_flush_slots | (Dreno antigo) onde o dreno no fim teria esperado primeiro, por motivo (no máximo um por submissão) |
| b_dry_late_parts, b_dry_late_mb, b_dry_late_views | (Dreno antigo) atualizações que teriam esperado o fim, os seus bytes, e delas as que esperariam por outra vista |
| b_dry_queued, b_dry_queued_ms | (Dreno antigo) outros passos que teriam esperado o fim, e o tempo deles |
| b_dry_overlap_ms | (Dreno antigo) preparo antes do primeiro ponto de dreno, sem o que esperaria o fim: o que teria corrido junto com a GPU |
| b_dry_after_flush_ms | (Dreno antigo) preparo depois do primeiro ponto de dreno |
| b_dry_wait_ms | (Dreno antigo) a espera de antes do preparo (pré-atualização e dreno) |
| b_dry_saved_ms | (Dreno antigo) por submissão, o menor entre a espera e o preparo que correria junto: a estimativa do tempo de GPU parada que o dreno no fim tira |
| b_dry_lw_inplace | (Dreno antigo) voos no ar cujo pouso escreve memória usada no lugar, ali ou em outra vista (decide o passo 3) |
| b_dry_decl_overlap | (Dreno antigo) declarações de escrita que o pouso no ar também escreve, ali ou em outra vista (decide o passo 3) |

Como ler: com o dreno no fim, gpu_idle_ms deve cair e wait_execute_ms ir a
quase zero (sobram só as submissões do dreno antigo); a espera que sobra
aparece em wait_tail_ms e wait_flush_ms. Se b_flush_copy for perto de
b_prepared_under e b_after_flush_ms grande, as cópias do jogo vêm cedo nas
submissões e quase todo o preparo volta para a GPU parada: aí o próximo passo
é o 4 (cópias sem dreno, abaixo). b_flush_slots deve ficar perto de 1 por
quadro (o olhar de uma vez por quadro acha quase sempre alguma vista solta)
e passar disso só quando o plano de descritores estoura o tempo (o jogo traz
milhares de slots de uma vez); com b_after_flush_ms grande ao mesmo tempo, o
resto daqueles preparos voltou para a GPU parada, e vale estreitar a regra
(só os slots cuja memória mudou de fato). b_late_views mostra quanto da
espera vem de vistas repetidas: perto
de b_late_parts, a memória com vistas é grande, e o passo 2 deve contar com
isso. prewarm_ms cai para perto de zero com o dreno no fim (só as
submissões que ficam no dreno antigo, e VYCE_TAIL_PREWARM=1, a rodam).
cpu_prepare_ms continua contando só o trabalho da CPU (as esperas do preparo
vão para gpu_wait_ms). Com o dreno no
fim ligado (o padrão) os b_dry_* ficam em 0; para vê-los numa sessão, rode
com VYCE_TAIL_DRAIN=0 e a telemetria ligada. Ali b_dry_prepared perto do
número de submissões do jogo por quadro e b_dry_saved_ms perto de
cpu_prepare_ms dizem que quase todo o preparo correria com a GPU
trabalhando (é uma conta por baixo: não conta que o jogo, liberado antes,
grava a submissão seguinte mais cedo); b_dry_flush_copy alto e
b_dry_after_flush_ms grande dizem que as cópias do jogo comem o ganho.

Testado pelo heavy.sh: victory-gpu na placa com 214 passando e 9 ignorados
(196 e 9 em 5c99db2, 205 antes da revisão) com o dreno no fim, com
VYCE_TAIL_DRAIN=0, com VYCE_TAIL_DRAIN=0 e VYCE_TELEMETRY=1 (a contagem a
seco em todo teste do dreno antigo), com VYCE_TAIL_DRAIN_STRICT=1 e com
VYCE_TAIL_PREWARM=1 (os testes novos estão em tail/tests.rs, watch.rs e
shadow.rs), e a build release. O teste principal roda 2.500 casos aleatórios
no WARP e 500 na RTX 2070 SUPER (VYCE_TAIL_SEEDS muda o número): de 2 a 6
submissões com atualizações, escritas e leituras de shaders numa cópia
esparsa e numa não esparsa de memória privada e nas cópias não esparsas das
duas vistas de uma seção de páginas físicas (a mesma memória em dois
endereços, como as do xmem), memória usada no lugar (uma parte numa das
vistas), preenchimentos, trabalho do runtime, cópias do jogo,
CopyMemoryToMemoryX, leituras de volta, escritas de fim de lista, listas
reenviadas e escritas da CPU do jogo entre as submissões, por qualquer das
vistas, com a GPU fazendo o seu trabalho quando a submissão é enviada ou só
quando a seguinte vai esperar por ela (o pior caso), ou depois de um tempo
ao acaso. A memória do jogo e tudo o que os shaders leram saem idênticos byte
a byte com o dreno antigo, com o dreno no fim e com o estrito. As cópias
também, byte a byte, nos casos em que nenhuma porta abre por tempo e o dreno
no fim roda com a pré-atualização (um caso em três); nos outros, os bytes das
cópias que nenhum passo pede dependem dela (só roda com um voo no ar e junta
pedaços a menos de 4 KiB; já com o dreno antigo, numa rodada de 400 na placa,
o mesmo caso deixou um desses bytes com dois valores). Em todo caso,
atualizadas no fim, as cópias são a memória do jogo. Num caso de cada dois o
dreno antigo roda com a contagem a seco ligada, sem mudar nada, e onde
nenhuma porta abre por tempo ela prevê exatamente o que o dreno no fim faz:
que submissões vão sob o voo, que atualizações esperam o fim e onde fica a
primeira espera. Com cada regra desligada (as atualizações, os passos
deixados para depois, as cópias do jogo, as cópias não esparsas, as marcas de
fence, as outras vistas) algum caso de no máximo 500 sai diferente, e o mesmo
caso com todas as regras sai igual ao do dreno antigo. Foi esse teste que
achou o vão da cópia de referência (acima), num caso de 2.500, quando a
pré-atualização saiu. Testes à parte, com e sem a regra de cada um: uma
atualização pela outra vista do que o pouso escreve e de memória usada no
lugar, os slots que o plano anterior deixou por fazer e os que o olhar acha,
rebotes de constantes, escritas pelos slots de acesso desordenado, a
pré-atualização (sem a chave e com ela) e a referência que retoma o vão (sem
isso a escrita da GPU se perde); e a devolução das cópias de referência, as
marcas, a espera no fim de fato depois do preparo, as outras vistas de uma
faixa e o limite de 64 vistas. A camada de depuração do D3D12 e a validação
na GPU não rodaram (o d3d12SDKLayers.dll não está instalado neste PC), nem um
teste de remoção do dispositivo no meio de um preparo. Ainda não foi medido
em jogo.

### Próximos passos (só desenho, não feitos)

Os números da próxima sessão dizem a ordem. Cada um com chave própria,
desligado primeiro.

- Passo 2, cópias de referência antes da espera (VYCE_TAIL_DRAIN_SNAP): dividir
  `keep_before` em dois. As vistas cujos blocos nenhum pendente do voo no ar
  toca e que não estão entre as atualizações deixadas para depois são copiadas
  antes da espera; o resto depois. Depende da devolução por voo (feita) e de o
  voo só escrever as faixas que declarou. O "nenhum pendente toca" vale também
  para os mesmos bytes em outra vista (`watch::other_views`), como na regra
  das atualizações. Tira até ~20 ms de cópias de referência por quadro do
  caminho parado (snapshot_ms).
- Passo 3, entrega sem esperar o pouso: a Line ganha uma fence "pousado" que o
  pouso sinaliza da CPU logo depois de `finish`; a próxima submissão espera na
  fila (`queue.Wait`) por ela só se o pouso no ar escreve memória usada no
  lugar (b_dry_lw_inplace) e espera o pouso na CPU só para as atualizações
  tardias, as declarações que se cruzam com pendentes do voo
  (b_dry_decl_overlap) e páginas que o jogo escreveu sob uma faixa pendente
  desde a última cópia (o diário de escrita; NOT_WATCHED conta como escrita).
  Precisa de um anel de listas Early (4), um anel da lista "antes" do card (3),
  PAIRS 8 para 16, até 3 voos e um piso de descritores dinâmicos livres.
- Passo 4, cópias do jogo sem dreno (se b_flush_copy for alto): primeiro uma
  forma só de CPU, fazer a cópia na hora quando nem a origem nem o destino
  tocam o que o pouso no ar escreve, o destino não é memória usada no lugar e
  nenhuma atualização deixada para depois nem passo na fila lê o destino; e
  daí em diante deixar para depois toda atualização que toca o destino (o voo
  pode estar lendo a cópia dele). Depois, para as que tocam memória usada no
  lugar, `CopyBufferRegion` numa lista na frente da submissão por um buffer
  intermediário, com transições explícitas, e os destinos avisados ao vigia de
  escrita no pouso.
