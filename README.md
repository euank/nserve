# nserve

nserve is a command line tool to easily create a public ngrok url (optionally
with oauth) for any program which runs and then listens on some port or ports
for HTTP.

nserve is meant to be a simpler way to run the combination of "ngrok + something" during local development.

nserve works best with programs like `rails serve`, `godoc -http=:6060`, and similar.

`nserve` will, by default, wrap a program to serve the first tcp port it opens on an ngrok url.

If the first port is incorrect, or it's tcp rather than http, options may be used to further configure that.

### Usage

`nserve -- [command]`

For example, `nserve -- rails serve` will run `rails serve` on a public ngrok url (printed as the first line of output, and available via the escape sequence below later).

nserve also supports the following additional optional flags:

```
nserve --open -- [command] # open the ngrok url in your browser

nserve --port 3000 -- [command] # serve the specific port

nserve --url "https://something.ngrok.io" -- [command] # use the specified ngrok url

nserve --google-oauth ".*@something.com" -- [command] # require google login with an email matching the provided regex.

nserve --tcp -- [command] # use tcp
```

These flags may be combined where the selected protocol supports them.

After running nserve, the first line output will be the ngrok url in use:

```
[nserve] Created ngrok session with URL https://random.ngrok.app
<regular command output>
```

At any time in the future, you may use the nserve escape sequence, <ctrl+N>, to get additional information:

| sequence | result |
|-------|---------|
| `ctrl+n+n` | send `ctrl+n` to the underlying process |
| `ctrl+n+?` | display help screen |
| `ctrl+n+s` | display nserve status page |

The status page displays the url, ngrok session status, and a log of requests + status codes.

## Build and run

nserve uses the ngrok Rust SDK directly. It does not install, invoke, or communicate
with the ngrok command-line binary.

```sh
cargo build --release
export NGROK_AUTHTOKEN=your-token
target/release/nserve -- your-server-command
```

Authentication is resolved in this order:

1. the `NGROK_AUTHTOKEN` environment variable; then
2. `~/.config/ngrok/ngrok.yml`.

Both the version 2 top-level `authtoken` field and the version 3
`agent.authtoken` field are supported. A missing config file is ignored, while
a present but unreadable or malformed file produces an error. nserve reads the
file itself and still does not invoke the ngrok executable.

Automatic port discovery currently requires Linux, procfs, and permission to trace
a process that nserve starts. If ptrace is unavailable (for example, because of a
container security policy), pass `--port`. Explicit-port mode does not use ptrace.

Google OAuth is implemented as an ngrok Traffic Policy. The supplied pattern uses
ngrok's RE2-compatible expression syntax. Since OAuth is an HTTP edge action,
`--google-oauth` and `--tcp` cannot be combined.
