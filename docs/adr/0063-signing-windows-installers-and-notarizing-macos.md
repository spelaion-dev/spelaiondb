# ADR-063: Sign the Windows installers through SignPath Foundation, and sign and notarize the macOS app with an Apple Developer ID

- **Status:** Accepted (not implemented: the accounts are the owner's to create first)
- **Date:** 2026-11-22
- **Amends:** [ADR-021](0021-automatic-updates.md) (the release signs the installers before
  the updater signs its artifacts; its "Revisit if" about Authenticode and notarization) and
  [ADR-012](0012-ci-as-reviewer.md) (its "Signing" line, which this ADR details)
- **Builds on:** ADR-057 (installers carry the plugins as archives, so signing changes no pinned
  program)

## Context
The releases are not signed by the operating systems' rules. The README tells people that
SmartScreen may ask on Windows and that macOS needs a workaround the first time. `tauri.conf.json`
signs the macOS app ad hoc (`"signingIdentity": "-"`), and `release.yml` signs only the update
artifacts, with the updater's minisign key (ADR-021). SpelaionDB holds database passwords and SSH
keys: a warning from the operating system is the first thing a careful user or a company's IT
department sees.

What each option asks for today (checked on 2026-11-22; the sources are at the end):

- **macOS has one way:** an Apple Developer Program membership, 99 USD a year. An individual
  enrols with an Apple ID. An organisation needs a legal entity and a D-U-N-S number, and
  nonprofits can ask for the fee to be waived. A Developer ID Application certificate signs the
  app with the hardened runtime, Apple's notary service checks it, and the ticket is stapled.
  Tauri's bundler does all of this when the `APPLE_*` variables are set, before it writes the
  updater's `.app.tar.gz`.
- **macOS 15 (Sequoia) took away the old workaround:** right-click → Open no longer opens an app
  that is not notarized. The user has to try to open it, then go to System Settings → Privacy &
  Security → Open Anyway and confirm with a password. The README's instruction is wrong on
  current macOS.
- **Windows, Azure Artifact Signing (formerly Trusted Signing):** about 9.99 USD a month, with
  short-lived certificates in Microsoft's HSM and no key to keep. It is open to individuals only
  in the USA and Canada. Organisations qualify in the USA, Canada, the EU and the UK, and need a
  paid Azure subscription. The owner is an individual in Spain, so this needs a company first.
- **Windows, an OV or EV certificate from a certificate authority:** since 2023 the private key
  has to live in hardware or a cloud HSM (no `.pfx` in a secret). Since 1 March 2026 a
  certificate lasts at most 460 days, so it is renewed and paid again every year or so. **EV no
  longer gives immediate SmartScreen reputation:** Microsoft treats EV and OV alike, and
  reputation builds per file over time. The cheapest option for an individual open-source
  developer is Certum's "Open Source Code Signing" (from about 69 EUR, key in its SimplySign
  cloud). SimplySign asks for a one-time code from a phone app to open a session, which does
  not fit an unattended CI run. A certificate on a CI-usable cloud HSM (DigiCert KeyLocker,
  SSL.com eSigner and similar) costs more, a few hundred USD a year. Check the price before
  buying.
- **Windows, SignPath Foundation:** free for open-source projects. It requires an OSI license
  and no proprietary components, an active, released and documented project, MFA for every
  member on GitHub and SignPath, a code-signing policy published in the README, and a build on
  GitHub-hosted runners that SignPath can trace to the commit. Every signing request is
  approved by hand in SignPath's dashboard. The certificate is the Foundation's, so Windows
  names **"SignPath Foundation"** as the publisher, not Spelaion. The key never leaves
  SignPath's HSM, and there is nothing to renew.
- **SmartScreen:** no option makes the warning disappear on the first day. Signing gives the
  dialog a publisher and lets reputation build. Reputation is per file and grows with
  downloads, so each new version starts with less of it than a long-lived file would.

## Decision
1. **Sign and notarize the macOS app first, with an Apple Developer ID, the owner enrolled as
   an individual.** Recent macOS makes an app that is not notarized hard to open at all, so this
   is the larger gain. Enrolling as an organisation, so that the certificate reads "Spelaion"
   rather than the owner's name, waits until there is a legal entity. Moving later means a new
   Team ID and certificate, which only changes the signature of new releases.
   - The release builds sign with `APPLE_SIGNING_IDENTITY`, use the hardened runtime, and
     notarize with an App Store Connect API key (`APPLE_API_ISSUER`, `APPLE_API_KEY`, and the
     `.p8` written to a file for `APPLE_API_KEY_PATH`), not with an Apple ID password.
   - Local and CI test builds keep signing ad hoc: nothing changes for contributors.
   - Before it ships, the first signed build checks two things. First, that notarization
     accepts the plugin archives the app carries in `Resources/plugins` (ADR-057), which hold
     unsigned Intel programs; if the notary service rejects them, the plugins' own releases
     sign their macOS programs with the same identity. Second, that the hardened runtime breaks
     nothing (the web view, the plugins started as child processes, the bundled `spelaiondb`).
2. **Sign the Windows installers through SignPath Foundation.** It is the only option that is
   free, keeps the key out of our hands, and fits an individual outside North America. Its
   costs are a publisher name that is not ours and one manual approval per release; both are
   acceptable for a preview. `.github/CODEOWNERS` already requires a person on `release.yml`.
   - Signing goes in two steps, so that the app inside the installer is signed too. The job
     builds the app's binaries and SignPath signs them: the app's executable and the bundled
     `spelaiondb.exe`. Then Tauri bundles the NSIS installer and the `.msi` around the signed
     programs, and SignPath signs those. Last, the updater's minisign signature is computed
     over the signed installer (`tauri signer sign`) and `latest.json` is written.
     `tauri-action` cannot do this in one call, because SignPath signs outside the bundler.
   - The CLI archives' `spelaiondb.exe` is signed in the same request as the app's.
   - **Fallbacks, in this order,** if SignPath does not accept the project or stops the
     programme: Azure Artifact Signing, once a company exists in the EU (signed through Tauri's
     `signCommand`, no manual step); otherwise an OV certificate on a cloud HSM that CI can use.
     Certum's SimplySign certificate is the last resort, because each release would have to be
     signed by a person at the keyboard.
3. **The updater's signature stays as it is (ADR-021), and comes last.** Authenticode and
   notarization tell the operating system who published a file. The minisign signature tells
   the app that an update is ours. The updater checks only the minisign signature, and keeps
   doing so. The installers are signed before the `.sig` files are computed, so each `.sig`
   covers the bytes users download. `latest.json` keeps its asset names and the updater its
   public key. On macOS the bundler already works in this order (sign, notarize, staple, then
   archive and sign for the updater).
4. **No signing key in the repository.** On Windows there is none to keep: it stays in
   SignPath's HSM. On macOS Apple offers no hosted signing for Developer ID, so the `.p12`, its
   password and the `.p8` are secrets of a GitHub **environment** named `release`. Only
   `release.yml` runs on a `v*` tag, and the owner is its required reviewer. The owner keeps
   copies in a password manager. A leaked macOS certificate is revoked in Apple's portal; a
   leaked API key is revoked in App Store Connect.
5. **The README says what is true now.** macOS: on macOS 15 and later, try to open the app, then
   System Settings → Privacy & Security → Open Anyway; on earlier versions, right-click → Open.
   Windows: SmartScreen may still ask until enough people have run a version. Each line changes
   when its platform is signed.

## What the owner must create or pay, in order
Accounts and payments are the owner's. The agents then do the implementation in the work items
below.

1. **Apple:** enrol in the Apple Developer Program as an individual (an Apple ID with two-factor
   authentication) and pay 99 USD. It is renewed yearly.
2. **Apple:** in Certificates, Identifiers & Profiles, create a **Developer ID Application**
   certificate (the CSR from Keychain Access on a Mac, or from `openssl`). Export it with its
   key as a `.p12` with a strong password.
3. **Apple:** in App Store Connect → Users and Access → Integrations, create a **team API key**
   with the Developer role. Download the `.p8` (it can be downloaded only once) and note the
   Issuer ID and the Key ID.
4. **GitHub:** create the environment `release` in the repository settings, limited to `v*`
   tags, with the owner as required reviewer. Add `APPLE_CERTIFICATE` (the `.p12` in base64),
   `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY` (the certificate's full name),
   `APPLE_API_ISSUER`, `APPLE_API_KEY` (the Key ID) and `APPLE_API_KEY_P8` (the `.p8`'s text).
   Keep copies of all of them in a password manager.
5. **SignPath:** make sure every member of the GitHub organisation has MFA, and merge the
   README's code-signing policy (work item C). Then apply to SignPath Foundation's open-source
   programme for `spelaion-dev/spelaiondb`.
6. **SignPath, once accepted:** follow their onboarding (organisation, project, signing policy,
   the GitHub integration), add the API token as the `release` environment's
   `SIGNPATH_API_TOKEN`, and give the agents the organisation ID and the project and policy
   slugs. Approve each release's signing requests in SignPath's dashboard.

Nothing here costs more than the Apple membership, unless SignPath declines and a fallback is
bought.

## Work items (Plane Backlog)
- **A. Sign and notarize the macOS app in `release.yml`** (SDB-442; CODEOWNERS): the `release`
  environment, the `APPLE_*` variables, the `.p8` written to a file and removed, and
  `signingIdentity` set only by the release config. Check the plugin archives and the hardened
  runtime as in decision 1. Needs owner steps 1–4.
- **B. Sign the Windows installers through SignPath in `release.yml`** (SDB-443; CODEOWNERS): the two
  signing requests, the updater signature computed after them, `latest.json` written with the
  same names, and the CLI's `spelaiondb.exe` signed. Needs owner step 6.
- **C. Publish the code-signing policy SignPath asks for** (SDB-444) in the README: who builds, who
  approves, what is signed, the privacy statement and the attribution. Needed before owner
  step 5.
- **D. The README's install lines once each platform is signed** (SDB-445), and the "Revisit if" of
  ADR-021 marked done.

## Consequences
- macOS users open the app with a double click. The developer name the system shows is the
  owner's until an organisation enrols.
- Windows users see "SignPath Foundation" as the publisher instead of "Unknown publisher".
  SmartScreen may still warn on a new version for a while.
- A release needs the owner twice: approving the `release` environment, and approving the
  SignPath requests. A release cannot be published while the owner is unavailable.
- One more yearly renewal (Apple). A lapsed membership does not break installed copies:
  notarized releases stay valid, but new releases cannot be notarized.
- The Linux packages and the AppImage stay unsigned by any operating system's rules; their
  updates are still checked with the minisign signature.

## Alternatives
- **Stay unsigned and keep the README's instructions:** free, but on current macOS the app is
  hard to open at all, and an unsigned installer is the first thing an IT department rejects.
  Refused.
- **Windows first:** SmartScreen can be clicked through; Gatekeeper on macOS 15 cannot without a
  trip to System Settings. macOS first.
- **An EV certificate for instant SmartScreen trust:** EV no longer gives it, and it costs the
  most. Refused.
- **A `.pfx` in a repository secret:** certificate authorities no longer issue exportable code
  signing keys, and AGENTS.md keeps private keys out of the repository. Refused.
- **Azure Artifact Signing now:** not open to an individual in Spain. It is the first fallback
  once a company exists.

## Revisit if
SignPath Foundation declines the project or ends the programme (use the fallbacks in decision
2). A legal entity is formed (enrol Apple as the organisation, and consider Azure Artifact
Signing so the publisher reads "Spelaion"). Microsoft changes what earns SmartScreen reputation,
or Apple changes how Developer ID apps are notarized.

## Sources (checked 2026-11-22)
- Apple: [membership and fee waivers](https://developer.apple.com/support/fee-waiver),
  [opening an app from an unknown developer on macOS 15](https://support.apple.com/guide/mac-help/open-a-mac-app-from-an-unknown-developer-mh40616/15.0/mac/15.0),
  [Sequoia removes the right-click override](https://mjtsai.com/blog/2024/07/05/sequoia-removes-gatekeeper-contextual-menu-override).
- Microsoft: [code signing options for Windows apps](https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/code-signing-options)
  (Artifact Signing eligibility),
  [SmartScreen reputation](https://learn.microsoft.com/windows/apps/package-and-deploy/smartscreen-reputation)
  (EV no longer grants reputation by default; reputation is per file).
- CA/Browser Forum: [code signing baseline requirements](https://cabforum.org/working-groups/code-signing/requirements/)
  and [ballot CSC-31](https://cabforum.org/2025/11/17/ballot-csc-31-maximum-validity-reduction/)
  (460 days from 1 March 2026).
- Certum: [Open Source Code Signing on SimplySign](https://shop.certum.eu/open-source-code-signing-on-simplysign.html).
- SignPath Foundation: its eligibility terms and GitHub integration, as projects applying
  describe them, for example [one application write-up](https://zenn.dev/shm_7ec/articles/signpath-oss-code-signing?locale=en).
  Read signpath.org's current terms before applying.
- Tauri: [Windows code signing](https://v2.tauri.app/distribute/sign/windows/) (`signCommand`)
  and [macOS code signing](https://v2.tauri.app/distribute/sign/macos/) (the `APPLE_*`
  variables).
