# Stremio NZBHydra 2 Addon

Provides Usenet streams from [NZBHydra 2](https://github.com/theotherp/nzbhydra2) for Stremio. This addon allows you to search and stream movies and series using NZB files from your configured NZBHydra 2 indexer and NNTP servers.

> [!WARNING]
>
> Usenet support is still experimental in Stremio. Please read the [official announcement](https://blog.stremio.com/stremio-new-stream-sources-usenet-rar-zip-ftp-and-more/) for details.

## Usage

New to usenet? See the [The Basics](#the-basics) section below.

### Configuration

The addon requires the following configuration parameters:

### 1. `indexerUrl` (text)

- **Title:** Indexer URL
- **Description:** The base URL of your NZBHydra 2 instance. Example: `http://localhost:5076` or your remote NZBHydra 2 server address.
- **Required:** Yes
- **Example:** `http://localhost:5076` or `https://nzbhydra2.example.com`

### 2. `indexerApiKey` (password)

- **Title:** Indexer API key
- **Description:** Your NZBHydra 2 API key. You can find this in the NZBHydra 2 web interface under _Config > Main > API Key_.
- **Required:** Yes
- **Example:** `abcd1234efgh5678ijkl9012mnop3456`

### 3. `nntpServers` (text)

- **Title:** NNTP Servers (comma separated)
- **Description:** A comma-separated list of NNTP server addresses to use for downloading NZB files. Example: `news.example.com,news2.example.com`
- **Required:** Yes
- **Example:** `nntps://username:password@news.eu.easynews.com/4`


### The Basics

Usenet is a global distributed discussion system that also serves as a source for binary files and media content. To download content from Usenet, you need access to:

1. **An indexer** – This is a service (like NZBHydra 2) that lets you search for NZB files, which describe the content you want to download.
2. **A provider** – This is a Usenet service (NNTP server) that actually stores and delivers the files. You connect to it using the server address, and often a username and password.

#### Where to get NNTP servers

- **Commercial Usenet providers:** Most users get NNTP server access by subscribing to a Usenet provider. Popular options include [Easynews](https://www.easynews.com/), [Newshosting](https://www.newshosting.com/), [UsenetServer](https://www.usenetserver.com/), [Eweka](https://www.eweka.nl/), and many others. These providers offer paid plans with high retention and speed.
- **Free servers:** Some ISPs or universities may offer free NNTP access, but these are rare and often limited.


**What you need to use this addon:** When you sign up, you’ll receive server addresses, a username, and a password. Enter the server addresses (comma separated if you have multiple servers) in the addon configuration.

#### More Usenet info

- Usenet is not the same as torrents or direct downloads. You need a provider and usually a subscription.
- Retention refers to how long files are kept on the server. Higher retention means more available content.
- For more details, see guides like [Usenet 101](https://www.usenet.com/what-is-usenet/) or your provider’s help pages.

## Developers

### Requirements

- [Node.js](https://nodejs.org/) (v24 or higher is recommended)
- [PNPM](https://pnpm.io/) package manager v9

### Installation

1. **Clone the repository:**
   ```zsh
   git clone https://github.com/sleeyax/stremio-nzb-addon.git
   cd stremio-nzb-addon
   ```
2. **Install dependencies:**
   ```zsh
   pnpm install
   ```
3. **Run the addon:**
   ```zsh
   npm run dev
   ```
   The addon will start a local server. Add the manifest URL to Stremio to use the addon.

## License

[MIT](./LICENSE)
