# autter Extension for Visual Studio

A Visual Studio extension that tracks AI-generated code using [autter](https://github.com/autter-dev/autter-cli?tab=readme-ov-file#quick-start).

## Install

The [autter quickstart](https://github.com/autter-dev/autter-cli?tab=readme-ov-file#quick-start) install script should automatically install the Visual Studio extension. If that didn't work or you'd like to install manually:

1. **Install the extension** from the [Visual Studio Marketplace](https://marketplace.visualstudio.com/items?itemName=autter.autter-visualstudio), or search for `autter` in Extensions > Manage Extensions.
2. **Install [`autter`](https://github.com/autter-dev/autter-cli)** Follow the `autter` installation [instructions](https://github.com/autter-dev/autter-cli?tab=readme-ov-file#quick-start) for your platform.
3. **Restart Visual Studio**

## Requirements

- Visual Studio 2022 (17.0+)
- autter CLI >= 1.0.23

## Debug logging

The extension logs detection events to the Visual Studio Output window (Debug pane). Look for lines prefixed with `[autter]` to see:

- Which files are being tracked
- Whether edits were detected as AI or human
- Checkpoint success/failure status

## Development

### Build

```bash
dotnet build src/AutterVS/AutterVS.csproj
```

Or open `AutterVS.sln` in Visual Studio and build from the IDE.

### Debug

1. Open `AutterVS.sln` in Visual Studio 2022
2. Set `AutterVS` as the startup project
3. Press F5 to launch an Experimental Instance with the extension loaded

### Package

```bash
dotnet build src/AutterVS/AutterVS.csproj -c Release
```

The `.vsix` file will be in `src/AutterVS/bin/Release/`.

## License

MIT
