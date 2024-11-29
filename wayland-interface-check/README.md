# Wayland Interface Check

<a href="https://crates.io/crates/wayland-interface-check">![Crates.io Version](https://img.shields.io/crates/v/wayland-interface-check)</a>

This simple binary answers the question, "Is this Wayland interface available?" For example,

```sh
$ wayland-interface-check zwlr_data_control_manager_v1
$ echo $?
0
```

means the interface is available.
