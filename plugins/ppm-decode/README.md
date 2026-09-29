# PPM decoder plugin

This is the first concrete consumer of Histima's registered-Wasm plugin ABI.
`ppm_decode.c` has no runtime imports and the checked-in `ppm_decode.wasm` is
the artifact embedded by the Tima runtime.

Rebuild it with the repository's preferred LLVM toolchain from the repository
root:

```powershell
& 'C:\Program Files\LLVM\bin\clang.exe' `
  '--target=wasm32' '-O2' '-nostdlib' '-ffreestanding' '-fno-builtin' `
  'plugins/ppm-decode/ppm_decode.c' `
  '-Wl,--no-entry' '-Wl,--export-memory' `
  '-Wl,--initial-memory=131072' '-Wl,--max-memory=67108864' `
  '-Wl,--strip-all' '-o' 'plugins/ppm-decode/ppm_decode.wasm'
```

The runtime validates the ABI version, rejects all imports, meters execution,
and caps linear memory independently of the module's declared maximum.
