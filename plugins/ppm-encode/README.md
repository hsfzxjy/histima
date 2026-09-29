# PPM encoder plugin

This is the encoding half of the first concrete consumer of Histima's
registered-Wasm plugin ABI. `ppm_encode.c` reads an immutable RGBA8 image view,
including its explicit stride, and returns deterministic ASCII P3 bytes.

Rebuild the checked-in artifact from the repository root:

```powershell
& 'C:\Program Files\LLVM\bin\clang.exe' `
  '--target=wasm32' '-O2' '-nostdlib' '-ffreestanding' '-fno-builtin' `
  '-Iplugins/include' `
  'plugins/ppm-encode/ppm_encode.c' `
  '-Wl,--no-entry' '-Wl,--export-memory' `
  '-Wl,--initial-memory=131072' '-Wl,--max-memory=67108864' `
  '-Wl,--strip-all' '-o' 'plugins/ppm-encode/ppm_encode.wasm'
```

The runtime performs the same import, fuel, memory, descriptor, and result
validation as it does for the decoder.
