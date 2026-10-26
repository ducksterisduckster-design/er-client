chcp 65001
if exist er_archipelago.dll.pending (
    copy /Y er_archipelago.dll.pending er_archipelago.dll
    if not errorlevel 1 del er_archipelago.dll.pending
)
.\bin\me3.exe launch -p me3-config.me3
