# OpenRGB Link for Ectomatter

Lighting that OpenRGB controls, such as your RAM.

This is a plugin for Ectomatter, one app for the devices on your desk. A plugin brings something into Ectomatter that isn't a device, such as another program, the games on the computer or the weather. It runs inside Ectomatter as a WebAssembly module and reaches only what its manifest (`plugins/openrgb-link/core.json`) names.

It is independent: not made, endorsed or supported by the OpenRGB project.

## Adding it

In Ectomatter's Settings, under Plugins, choose **Add a plugin**, then pick OpenRGB Link and press **Add**. Ectomatter shows what the plugin can reach before you add it, and takes the package only if it matches the checksum in Ectonomic's list of reviewed packages, [ectomatter-packages](https://github.com/Ectonomic/ectomatter-packages).

## Building it

The folders are laid out as in Ectomatter's own repository. To build the module:

```sh
rustup target add wasm32-wasip2
tools/build-module.sh plugins/openrgb-link/module/Cargo.toml plugins/openrgb-link/core.wasm
```

Each release's module is built from its tag this way, with Rust 1.98.1.

- `plugins/openrgb-link/`: the plugin itself: its manifest, the files it carries, and its module's source (`module/`)
- `crates/`: the libraries the module is built from
  - `ectomatter-protocol`: the shared names and actions of Ectomatter's core protocol
- `wit/core.wit`: the interface between the module and Ectomatter

## License

Copyright (C) 2026 Ectonomic LLC. Made by Grant Garrison and Claude.

This plugin is free software under the GNU General Public License, version 3 only (`LICENSE`): you may share and change it under its terms. It comes with no warranty.
