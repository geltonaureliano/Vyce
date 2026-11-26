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

A ação Run abre o jogo e o painel do Vyce. O painel mostra FPS, tempo e
desenhos por quadro, quadros apresentados, preparação da CPU, espera pelas
submissões da GPU, descartes e logs. A atividade de carregamento reflete
os eventos observados nos logs e no renderizador.

O painel atualiza CPU e RAM do sistema e do jogo a cada segundo. A RAM do
sistema mostra uso, total e memória disponível. A RAM do jogo corresponde
à memória residente. A CPU do jogo é medida como fração da capacidade
total da máquina. O painel também mostra o número de processadores
lógicos e de threads do jogo.

Use -NoDebugWindow para abrir só o jogo. Fechar o painel mantém a sessão
do jogo aberta.

Os dois registros continuam em Vyce-data\logs. Nas builds com telemetria,
o arquivo de erros também recebe uma linha VYCE_TELEMETRY por amostra para
permitir comparações posteriores. O painel omite essas linhas brutas da
visualização de logs.

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

Para abrir o painel de uma sessão já em execução sem reiniciar o jogo,
execute scripts\vyce-monitor.ps1 e passe -GamePid, -StdoutLog e -StderrLog.

## Progresso do carregamento

A imagem do jogo e o painel mostram em que etapa o jogo está, quanto já
carregou e quanto falta.

| Na imagem, abaixo do FPS | Quando |
| --- | --- |
| INICIANDO 35%  ~1 MIN | Do início do processo até o primeiro menu |
| CARREGANDO 43%  ~3 MIN | Na tela de carregamento, a primeira e as seguintes |
| FALTAM 231 PIPELINES  ~2 MIN | No mundo, enquanto muitos pipelines ainda são criados: faltam objetos e o FPS fica baixo |

Em menus e vídeos a linha some. Ela também aparece no título da janela.

A porcentagem compara este carregamento com os anteriores do mesmo tipo,
guardados em Vyce-data\cache\loads.txt (os seis últimos de cada): o tempo que
passou e quanto já foi lido do disco. O que acertou mais das outras vezes
pesa mais. A porcentagem nunca volta e fica em 99% até o mundo aparecer.

Sem um carregamento anterior para comparar, ou com os caches frios depois de
trocar de build ou de driver, a linha fica assim:
CARREGANDO ~43%?  HA 3:25. A porcentagem é um palpite, tirado dos tempos das
primeiras sessões, e o tempo é o que já passou. Na vez seguinte o ? some.

VYCE_LOADING=0 esconde a linha na imagem (VICTORY_FPS=0 esconde só o FPS).
Apagar loads.txt recomeça o histórico. Linhas estragadas do arquivo são
ignoradas.

Com a telemetria ligada, o log de erros recebe uma linha VYCE_LOADING por
segundo, mesmo quando nenhum quadro é apresentado:

| Campo | Significado |
| --- | --- |
| t_s | Segundos desde o início do processo |
| stage | start (início), idle (menu ou vídeo), load (primeira tela de carregamento), again (as seguintes), world (no mundo) |
| percent, left_s | Porcentagem e segundos que faltam; null fora de um carregamento |
| known | false: a porcentagem é um palpite, sem carregamento anterior parecido |
| elapsed_s, next_s | Segundos nesta etapa; segundos que o mundo costuma levar (antes de ele começar a carregar) |
| read_mb, read_of_mb, read_mb_s | MiB lidos neste carregamento, quantos o anterior leu, MiB por segundo (média de 10 s) |
| readers | Leitores parados à espera do disco, em média nos últimos 10 s (há um por processador lógico) |
| reads, bytes_mb, written_mb, queued, files | Leituras de arquivos, MiB lidos e MiB entregues ao jogo desde o início, leituras na fila, arquivos abertos |
| frames, work | Quadros apresentados; desenhos executados e descartados |
| pipelines_waiting, pipelines_done, pipelines_left_s | Pipelines na fila, concluídos, e segundos até a fila acabar (no mundo, quando ela diminui) |
| shaders, cold | Shaders usados; true quando a maioria dos shaders ou pipelines é criada de novo (build ou driver novo) |

Para saber se o disco é o limite: readers em 4 ou mais com menos de 60 MB/s é
espera pelo disco. Abaixo de 1, quem trabalha é o próprio jogo. Ao fim de cada
carregamento o log traz a linha "a load is done" com a duração, o que foi lido
e os oito arquivos mais lidos.

## Pipelines guardados entre sessões

Os pipelines prontos ficam em Vyce-data\cache\pipelines\host-pipelines.bin e
são carregados na sessão seguinte. Isso agora vale também para os de mesh
shader (cerca de 550), que antes eram compilados de novo em toda sessão. Os
já guardados por builds anteriores continuam valendo. Um driver novo rejeita
o arquivo inteiro e a biblioteca recomeça vazia.

O arquivo passa de 150 MB e é escrito inteiro. A escrita acontece em segundo
plano, com a menor prioridade de disco: três minutos depois do primeiro
pipeline novo, ao fim de um carregamento e ao sair do jogo. Se o jogo travar,
os pipelines criados nos últimos minutos são criados de novo na vez seguinte.

## Campos novos de VYCE_TELEMETRY

| Campo | Significado |
| --- | --- |
| indirect_per_frame, indirect_dropped_per_frame | Execuções indiretas descartadas por quadro. São o mesmo número: o primeiro nome é o antigo e nunca contou as que rodaram |
| indirect_draws_per_frame, indirect_dispatches_per_frame | Execuções indiretas que rodaram, de desenho (com as de mesh) e de compute; cada uma conta uma vez, com quantos desenhos tiver |
| t_s | Segundos desde o início do processo, como em VYCE_LOADING |
| console, out_w, out_h, planes | Console informado ao jogo, tamanho da imagem apresentada e de quantas camadas ela é feita |
| scene_w, scene_h | Tamanho da cena na resolução temporal do jogo; 0 enquanto ela não roda |
