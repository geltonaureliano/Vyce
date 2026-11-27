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
| VICTORY_LDS_LOCAL=0 | Grupos de uma onda com memória compartilhada voltam ao fluxo do console (com uma barreira em cada troca) em vez de duas ondas locais de 32 |
| VICTORY_LOCAL_MEET_CHECKS=0 | As duas ondas locais voltam a se encontrar onde o fork as punha: também dentro de laços que cada thread deixa numa volta diferente, dentro de desvios em valores que as duas ondas podem ter diferentes (leitura de uma lane, inclusive de um acumulador como `v_fmac_f32` ou de uma seleção por máscara constante, e leitura escalar de memória que o próprio dispatch escreve) e em programas que acessam memória por endereço. Só para comparação: pode travar a GPU |
| VICTORY_LOOP_EARLY_OUTS=0 | As formas sem saída antecipada voltam a tirar também a saída de dentro de um laço, mesmo quando é a única saída dele (as threads giram até o limite de voltas). Só para comparação: pode travar a GPU |
| VICTORY_HOST_WAVE_READS=0 | Com as checagens acima ligadas, a análise das duas ondas locais volta a ser a de 1952aaf: um valor conta como igual em todas as lanes mesmo quando a instrução lê algo além dos operandos (o valor antigo de um acumulador como `v_fmac_f32`, `v_mac_f32` ou `v_dot4c_i32_i8`, o de uma troca, o VCC de `v_div_fmas_f32`) ou seleciona por uma máscara constante de algumas lanes, e uma leitura escalar conta como igual nas duas ondas mesmo de memória que o próprio dispatch escreve. Só para comparação: pode travar a GPU |
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
| VICTORY_NULL_COPY=0 | Uma cópia pedida pelo jogo à memcpy do VCRUNTIME140 a partir da página nula volta a encerrar o processo (hoje, só nesse caso estreito, o destino recebe zeros e a chamada volta; é a queda da missão do barco, aviso "a copy from the null page read as zeros") |
| VYCE_TAIL_DRAIN=0 | Cada submissão do jogo volta a esperar a anterior terminar antes de ser preparada, exatamente como antes (hoje é preparada com a anterior ainda na GPU e espera só antes de entregar as listas; ver Fase 2, abaixo) |

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
  pouso no ar escreve, faixas entregues aos shaders, slots de descritores
  feitos (o jogo só reescreve slots que nada em voo lê, como no console);
- deixado para depois da espera, na ordem em que vinha: atualização de cópias
  de memória que o pouso no ar escreve (o que os shaders dele escreveram nas
  cópias, as leituras de volta, as escritas de fim de lista e o que outras
  filas puseram atrás dele), de memória usada no lugar pela GPU (as escritas
  dela ali não são conhecidas) ou do que os shaders dele escrevem pelos slots
  de acesso desordenado das pilhas indexadas direto (isso fica na cópia, não
  volta); e todo passo que lê a memória do jogo para memória nossa ou escreve
  o que o voo ainda pode ler: preenchimentos, o trabalho do runtime, texturas
  e os seus envios, prelúdios, os rebotes de constantes e as cópias de
  referência de cópias que não são esparsas;
- uma espera onde está (ponto de dreno): cópias de buffers do jogo
  (`CopyBufferRegion`, `CopyResource`), `CopyMemoryToMemoryX`, e a declaração
  de escrita de shaders numa cópia não esparsa que o voo no ar também
  escreve. Dali em diante o preparo é o de antes.

Uma atualização deixada para depois contorna as faixas que a submissão já
tinha entregue aos shaders no passo dela, e só essas: depois do dreno antigo
eram as únicas fechadas. As faixas que os shaders escrevem levam agora o
valor de fence da submissão, e um pouso abre só as suas (antes eram tiradas
por valor, e o pouso da anterior abria a mesma faixa declarada pela
seguinte). As cópias de referência de blocos de uma cópia esparsa voltam ao
pool por voo, as que nada mais pendente toca (antes só quando nada estava
pendente; com o preparo sob o voo isso nunca aconteceria). A pré-atualização
de antes (prewarm) continua igual.

Ficam no dreno antigo, contados: submissões com uma lista ainda no ar
(reenviada pelo jogo) e as com shaders que escrevem buffers na VRAM (essas
esperam a GPU de qualquer jeito). O dreno no fim desliga sozinho, com uma
linha no log, com VICTORY_ASYNC_SUBMIT=0, VYCE_LATE_SIGNAL=0, VYCE_ONE_ECL=0,
VYCE_ONE_PASS=0, VICTORY_SWEEP_TILES diferente de 0 e, na build diag,
VICTORY_GPU_TIMING.

| Variável | Efeito |
| --- | --- |
| VYCE_TAIL_DRAIN=0 | Volta ao dreno antigo, exatamente: cada submissão espera tudo em voo antes de ser preparada |
| VYCE_TAIL_DRAIN_STRICT=1 | Modo paranoico: toda atualização de cópia espera o fim, não só as que o pouso escreve. Se uma imagem errada com o dreno no fim fica certa com este modo, faltou alguma escrita na lista do pouso ou da memória usada no lugar |
| VYCE_TAIL_DRAIN_DRY=0 | Com o dreno antigo e a telemetria ligada, não conta o que o dreno no fim (o normal, não o estrito) faria (os campos b_dry_* abaixo) |

O que não está coberto e continua como sempre esteve (só a ordem no tempo
pode mudar): escritas de shaders em cópias que ninguém declara (shaders que
escrevem por ponteiro; slots de acesso desordenado maiores que 8 MiB, ou
feitos desde que o plano de descritores foi montado inteiro pela última
vez) correm com a atualização da submissão seguinte quando o jogo escreve a
mesma página de 64 KiB; com o dreno antigo essas escritas sumiam sempre, agora
podem ficar. VYCE_TAIL_DRAIN_STRICT=1 devolve a ordem antiga para isso também.
Slots de descritores que o jogo reescreve enquanto um voo ainda os lê passam a
valer para esse voo (no console também valeriam); com o dreno antigo, não.

Campos novos de VYCE_TELEMETRY, por quadro:

| Campo | Significado |
| --- | --- |
| b_mode | 0 dreno antigo, 1 dreno no fim, 2 estrito (o maior da amostra) |
| b_prepared_under | Submissões do jogo preparadas com outra no ar |
| b_overlap_ms | Preparo feito antes da primeira espera: o tempo que a GPU deixou de ficar parada (no máximo) |
| wait_tail_ms, drains_tail | Espera no fim do preparo (a que sobra) e quantas esperaram de fato |
| wait_flush_ms, drains_flush | O mesmo nos pontos de dreno no meio do preparo |
| b_flush_copy, b_flush_run, b_flush_dense | Pontos de dreno por motivo: cópia do jogo, CopyMemoryToMemoryX, cópia não esparsa que o voo também escreve |
| b_after_flush_ms | Preparo depois da primeira espera (volta a ser tempo de GPU parada) |
| b_late_parts, b_late_mb | Atualizações de cópias deixadas para depois da espera, e os seus bytes |
| b_queued, b_queued_ms | Outros passos deixados para depois, e o tempo deles |
| b_old_list, b_old_card | Submissões que ficaram no dreno antigo: lista ainda no ar, shaders que escrevem buffers na VRAM |
| b_dry_prepared | (Dreno antigo) submissões que teriam sido preparadas sob o voo |
| b_dry_flush_copy, b_dry_flush_run, b_dry_flush_dense | (Dreno antigo) onde o dreno no fim teria esperado primeiro, por motivo (no máximo um por submissão) |
| b_dry_late_parts, b_dry_late_mb | (Dreno antigo) atualizações que teriam esperado o fim |
| b_dry_queued, b_dry_queued_ms | (Dreno antigo) outros passos que teriam esperado o fim, e o tempo deles |
| b_dry_overlap_ms | (Dreno antigo) preparo antes do primeiro ponto de dreno, sem o que esperaria o fim: o que teria corrido junto com a GPU |
| b_dry_after_flush_ms | (Dreno antigo) preparo depois do primeiro ponto de dreno |
| b_dry_wait_ms | (Dreno antigo) a espera de antes do preparo (pré-atualização e dreno) |
| b_dry_saved_ms | (Dreno antigo) por submissão, o menor entre a espera e o preparo que correria junto: a estimativa do tempo de GPU parada que o dreno no fim tira |
| b_dry_lw_inplace | (Dreno antigo) voos no ar cujo pouso escreve memória usada no lugar (decide o passo 3) |
| b_dry_decl_overlap | (Dreno antigo) declarações de escrita que o pouso no ar também escreve (decide o passo 3) |

Como ler: com o dreno no fim, gpu_idle_ms deve cair e wait_execute_ms ir a
quase zero (sobram só as submissões do dreno antigo); a espera que sobra
aparece em wait_tail_ms e wait_flush_ms. Se b_flush_copy for perto de
b_prepared_under e b_after_flush_ms grande, as cópias do jogo vêm cedo nas
submissões e quase todo o preparo volta para a GPU parada: aí o próximo passo
é o 4 (cópias sem dreno, abaixo). cpu_prepare_ms continua contando só o
trabalho da CPU (as esperas do preparo vão para gpu_wait_ms). Com o dreno no
fim ligado (o padrão) os b_dry_* ficam em 0; para vê-los numa sessão, rode
com VYCE_TAIL_DRAIN=0 e a telemetria ligada. Ali b_dry_prepared perto do
número de submissões do jogo por quadro e b_dry_saved_ms perto de
cpu_prepare_ms dizem que quase todo o preparo correria com a GPU
trabalhando (é uma conta por baixo: não conta que o jogo, liberado antes,
grava a submissão seguinte mais cedo); b_dry_flush_copy alto e
b_dry_after_flush_ms grande dizem que as cópias do jogo comem o ganho.

Testado pelo heavy.sh: victory-gpu na placa com 205 passando e 9 ignorados
(196 e 9 em 5c99db2), com o dreno no fim, com VYCE_TAIL_DRAIN=0, com
VYCE_TAIL_DRAIN=0 e VYCE_TELEMETRY=1 (a contagem a seco em todo teste do
dreno antigo) e com VYCE_TAIL_DRAIN_STRICT=1 (os 9 testes novos estão em
tail/tests.rs), e a build release. O teste principal
roda 2.500 casos aleatórios no WARP e 500 na RTX 2070 SUPER (VYCE_TAIL_SEEDS
muda o número): de 2 a 6 submissões com atualizações, escritas e leituras de
shaders numa cópia esparsa e numa não esparsa da mesma memória, memória usada
no lugar, preenchimentos, trabalho do runtime, cópias do jogo,
CopyMemoryToMemoryX, leituras de volta, escritas de fim de lista, listas
reenviadas e escritas da CPU do jogo entre as submissões, com a GPU fazendo o
seu trabalho quando a submissão é enviada ou só quando a seguinte vai
esperar por ela (o pior caso), ou depois de um tempo ao acaso. A memória do
jogo e tudo o que os shaders leram saem idênticos byte a byte com o dreno
antigo, com o dreno no fim e com o estrito. As duas cópias também, byte a
byte, nos casos em que nenhuma porta abre por tempo; nos outros, os bytes
das cópias que nenhum passo pede dependem, já com o dreno antigo, de o voo
anterior ter pousado ou não antes da pré-atualização (que só roda com um voo
no ar e junta pedaços a menos de 4 KiB): o mesmo caso, só com o dreno
antigo, deixou um desses bytes com dois valores em 400 rodadas na placa. Em
todo caso, atualizadas no fim, as cópias são a memória do jogo. Num caso de
cada dois o dreno antigo roda com a contagem a seco ligada, sem mudar nada,
e onde nenhuma porta abre por tempo ela prevê exatamente o que o dreno no fim
faz: que submissões vão sob o voo, que atualizações esperam o fim e onde
fica a primeira espera. Com cada regra desligada (as atualizações, os passos
deixados para depois, as cópias do jogo, as cópias não esparsas, as marcas de
fence) algum caso de no máximo 500 sai diferente. Testes à parte: rebotes de
constantes, escritas pelos slots de acesso desordenado, a devolução das
cópias de referência, as marcas e a espera no fim de fato depois do preparo.
Ainda não foi medido em jogo.

### Próximos passos (só desenho, não feitos)

Os números da próxima sessão dizem a ordem. Cada um com chave própria,
desligado primeiro.

- Passo 2, cópias de referência antes da espera (VYCE_TAIL_DRAIN_SNAP): dividir
  `keep_before` em dois. As vistas cujos blocos nenhum pendente do voo no ar
  toca e que não estão entre as atualizações deixadas para depois são copiadas
  antes da espera; o resto depois. Depende da devolução por voo (feita) e de o
  voo só escrever as faixas que declarou. Tira até ~20 ms de cópias de
  referência por quadro do caminho parado (snapshot_ms).
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
