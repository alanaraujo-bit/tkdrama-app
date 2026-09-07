# FFmpeg sidecar

O instalador inclui `ffmpeg.exe` para remuxar, sem recodificação, o MPEG-TS do
ReelShort para MP4. O usuário não precisa instalar nada no sistema.

- versão: FFmpeg n9.0.1-27-g9b0578816c (build LGPL compartilhada)
- origem: https://github.com/BtbN/FFmpeg-Builds/releases
- arquivo de origem: `ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip`
- SHA-256 do executável: `276A98E9ACB5DC37C90D27402B89AC0ACECA90A84D485628708ED6CEB6F05913`
- licença distribuída: `FFMPEG-LICENSE.txt`

O nome com o target triple é exigido pelo empacotador do Tauri. No aplicativo
instalado, ele e as DLLs LGPL são copiados ao lado do executável principal. A
forma compartilhada mantém cada arquivo abaixo do limite comum de 100 MiB dos
servidores Git e permite substituir os componentes LGPL separadamente.
