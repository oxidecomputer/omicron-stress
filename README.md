# omicron-stress 

A stress testing framework for [omicron, the Oxide control
plane](https://github.com/oxidecomputer/omicron), because stress tests reduce
stress levels :)

## Usage

Run `omicron-stress --help` to see all usage options.

Before running stress, you need to start an Omicron cluster and log into it
(e.g. with the [Oxide CLI](https://github.com/oxidecomputer/oxide.rs)) to obtain
an API token for that cluster.

The runner will obtain the Nexus API URI from the following locations (evaluated
in order):

- The value of the `--host-uri` command line option
- The value of the `OXIDE_HOST` environment variable

The runner will then try to obtain a login token from the following sources
(again evaluated in order):

- A `hosts.toml` file stored in one of the following locations:
  - The value of the `--hosts-toml-dir` command line option
  - `$HOME/.config/oxide`
- The value of the `OXIDE_TOKEN` environment variable
