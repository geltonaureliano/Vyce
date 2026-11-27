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

Campos de VYCE_TELEMETRY desse trabalho (por quadro): gpu_idle_ms (GPU
parada entre submissões), wait_*_ms e drains_* (tempo e número de esperas
pela GPU por motivo: execute, signal, work, held, present, fence, list),
late_signals, late_waits, landed e landed_here (submissões encerradas, e
as encerradas por quem precisou antes da thread própria), ecl_calls,
host_lists, watch_asks, watch_ms e watch_ahead (perguntas ao kernel e
respostas do grupo), copies_joined, flights_max.

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
| VICTORY_COMPILE_HOLD=20,150,3,0 | O vigia das compilações sem fim volta aos limites de antes. Hoje 60,300,8,2: segundos até suspeitar, segundos até parar de qualquer jeito, GiB que o processo cresce, e GiB livres da máquina (os 32 GB daqui) abaixo dos quais para logo uma compilação com mais de 20 s, se o processo cresceu 1 GiB ou mais desde que ela é vigiada. Se cresceu menos, a falta de memória não é dela (o jogo carregando o mundo): o log avisa, ela segue e o pipeline não vai para a lista dos deixados de fora |
| VICTORY_SAVE_STRICT=1 | Uma versão de save que não pode ser apagada volta a impedir a leitura do container (o jogo então começava sem as configurações) |
| VICTORY_SHADER_OPT=all | Liga as passadas de limpeza dos shaders traduzidos (desligadas aqui: na RTX 2070 SUPER davam 0,2 % de código a menos por 2,3 vezes o tempo de tradução). Desligadas, o cache de shaders é o mesmo de antes delas; ligadas, a primeira sessão traduz tudo de novo. Um número escolhe as passadas (1, 2, 4, 8, 16), e VICTORY_SHADER_OPT_TRACE=1 mostra o que a de código morto tira |

Na compilação diag, VICTORY_SHADER_DUMP também salva os shaders dos
pipelines que o driver recusa, e a cada 30 s o log conta as texturas
posicionadas (quantas, quanto da memória do jogo cobrem, o que o pool e o
host guardam).

Para abrir o painel de uma sessão já em execução sem reiniciar o jogo,
execute scripts\vyce-monitor.ps1 e passe -GamePid, -StdoutLog e -StderrLog.
