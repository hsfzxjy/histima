# RGBA nearest-neighbor resize plugin

This registered-Wasm transform consumes an immutable rank-3 byte `Buffer`
with shape `[height, width, 4]` and returns a dense resized `Buffer` with the
same RGBA8 layout. For each output coordinate, the source coordinate is
`floor(output_coordinate * source_size / output_size)`.

Rebuild the checked-in artifact from the repository root:

```powershell
& 'C:\Program Files\LLVM\bin\clang.exe' `
  '--target=wasm32' '-O2' '-nostdlib' '-ffreestanding' '-fno-builtin' `
  '-Iplugins/include' `
  'plugins/rgba-resize-nearest/rgba_resize_nearest.c' `
  '-Wl,--no-entry' '-Wl,--export-memory' `
  '-Wl,--initial-memory=131072' '-Wl,--max-memory=67108864' `
  '-Wl,--strip-all' '-o' `
  'plugins/rgba-resize-nearest/rgba_resize_nearest.wasm'
```

Like the codec plugins, the module imports nothing and runs under the host's
fuel, memory, descriptor, and result validation.
