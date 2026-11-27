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
| -OutputSize 1920x1080 | Tamanho da imagem do jogo (a cena é 1/1,5 dele); keep deixa como está |
| -TitleSettings light | Desliga o que mais pesa numa placa de PC: filas assíncronas, borrão de movimento, menos detalhe ao longe e reflexos menores |
| -TitleSettings keep | Não muda nada além do tamanho |
| -TitleSettings original | Volta o arquivo como o jogo o criou |
| -TitleArguments a, b | Argumentos repassados ao jogo |

Ainda não se viu o jogo respeitar cada valor do arquivo: compare uma
sessão com -OutputSize keep -TitleSettings keep e procure no log as linhas
"presenting to a window" e "twins made".

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
| VICTORY_LOCAL_MEET_CHECKS=0 | As duas ondas locais voltam a se encontrar onde o fork as punha: também dentro de laços que cada thread deixa numa volta diferente, dentro de desvios em valores que as duas ondas podem ter diferentes e em programas que acessam memória por endereço. Só para comparação: pode travar a GPU |
| VICTORY_LOOP_EARLY_OUTS=0 | As formas sem saída antecipada voltam a tirar também a saída de dentro de um laço, mesmo quando é a única saída dele (as threads giram até o limite de voltas). Só para comparação: pode travar a GPU |
| VICTORY_STRIDE_ONCE=0 | A tabela de passos volta a ser lida a cada uso, não uma vez por vista e bloco |
| VICTORY_INTERP_F16_MODS=0 | Interpolações de 16 bits voltam a escrever só a metade baixa, sem multiplicador nem saturação |
| VICTORY_F16_NEAREST=1 | Resultados de 16 bits arredondados ao mais próximo, como no console (a Turing arredonda para zero); desligado por padrão, custa uma dúzia de instruções por resultado |

Cada uma dessas chaves do tradutor muda a chave do cache de shaders: com
uma delas trocada, todos os shaders são traduzidos de novo (o log avisa).

Para abrir o painel de uma sessão já em execução sem reiniciar o jogo,
execute scripts\vyce-monitor.ps1 e passe -GamePid, -StdoutLog e -StderrLog.
