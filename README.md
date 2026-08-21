## nserve

nserve is a command line tool to easily create a public ngrok url (optionally
with oauth) for any program which runs and then listens on some port or ports
for http.

nserve is meant to be a simpler way to run the combination of "ngrok + something" during local developlent.

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

These flags may all be combined arbitrarily.

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
| `ctrl+n+s | display nserve status page |

The status page displays the url, ngrok session status, and a log of requests + status codes.
