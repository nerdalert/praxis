---
issue: https://github.com/praxis-proxy/praxis/issues/664
discussion: https://github.com/praxis-proxy/praxis/issues/664
status: proposed
authors:
  - nerdalert
graduation_criteria:
  - Peer identity and gateway ingress trust requirements reviewed
    by stakeholders
  - Routing descriptor, route metadata, and request-path boundary
    reviewed by stakeholders
  - Future agent-protocol scope agreed separately from initial
    inference and MCP routing scope
  - Existing gateway-to-gateway E2E spike linked from follow-up
    implementation plan
  - Follow-up implementation PRs link any new E2E evidence they produce
  - Follow-up How? section added with implementation PR list after
    direction is accepted
stakeholders:
  - shaneutt
  - twghu
  - usize
---

# Gateway-to-Gateway Connectivity, Metadata, and Routing

## What?

Add the gateway-level primitives Praxis needs for mutually trusted
gateway-to-gateway communication. A Praxis gateway should be
able to route selected inference and tool requests to another
Praxis gateway when that remote gateway advertises an eligible
capability. The same trust and routing model should leave room
for later agent-oriented protocols.

This proposal is the initial direction document for
[epic #664]. It covers the gateway data plane and public
configuration surface only. Praxis consumes locally accepted
routing snapshots; the AI Grid Operator is expected to build and
publish those snapshots outside the request path. This proposal
defines how Praxis uses that state, not how the Operator computes
or distributes it.

[epic #664]: https://github.com/praxis-proxy/praxis/issues/664

### Goals

- Establish a trusted gateway-to-gateway traffic path using
  mutual TLS and explicit peer identity.
- Define the gateway identity facts needed for trust decisions,
  route decisions, logs, and later discovery surfaces.
- Let Praxis represent remote gateway sites and their advertised
  capabilities as validated local configuration or local routing
  snapshots.
- Represent enough gateway and capability information for workloads
  to discover which platform-designated gateway to call and what
  broad capabilities or purpose are available through that gateway.
- Route inference requests to a local backend or a remote gateway
  based on request facts, advertised capabilities, freshness, and
  locality.
- Keep inference routing API-shape neutral: supported request
  shapes such as Chat Completions or Responses should use the
  routing facts Praxis extracts, rather than separate
  gateway-to-gateway paths per endpoint.
- Route MCP tool traffic to a local backend or a remote gateway
  using protocol metadata that Praxis already extracts.
- Leave room for future agent-protocol routing once Praxis has the
  required request metadata extraction and policy model.
- Ensure internal routing metadata cannot be spoofed by public
  clients.
- Keep request-time routing decisions bounded, local, and
  observable.
- Preserve the existing separation between route selection and
  endpoint selection: gateway routing chooses a cluster or remote
  gateway, while the existing load balancer and local schedulers
  continue to choose concrete upstream endpoints.

In this proposal, freshness means whether a capability advertisement
is still considered valid by the locally accepted routing snapshot.
It is not a request-time probe or external control-plane lookup.

### Non-Goals

- Implementing the AI Grid Operator, Kubernetes controller, or
  snapshot distribution control plane inside Praxis.
- Querying the Operator, Kubernetes API, or any external state
  source on every request.
- Publishing workload discovery records through an Operator,
  Kubernetes resource, Gateway API object, DNS record, service
  catalog, or other platform mechanism.
- Replacing local inference schedulers or endpoint pickers such as
  llm-d.
- Defining the full policy engine for agents or tenants.
- Defining billing, quota ledgers, or durable commercial records.
- Sharing provider credentials or secrets between gateways.
- Implementing automatic failover after bytes may already have
  reached an upstream backend.

### Required Capabilities

**Remote gateway egress**

Praxis can connect to another Praxis gateway as an upstream, with
timeouts, health, load balancing, and upstream TLS.

**Gateway ingress trust**

Praxis can require mTLS on a gateway listener and identify the
authenticated peer gateway.

**Gateway identity**

Praxis can represent the local gateway identity and expose verified
peer identity to filters, logs, and route metadata without trusting
client-controlled input.

**Workload gateway discovery**

Praxis can represent gateway identity, purpose, and entrypoint
metadata for gateways designated to a workload by the AI platform.

**Capability discovery**

Praxis can represent bounded capability summaries, such as supported
inference or tool capabilities, so workloads can understand the
high-level capabilities or purpose available through one assigned
gateway or an aggregate of assigned gateways before sending traffic.

**Internal metadata boundary**

Public requests cannot set trusted gateway-routing metadata.
Gateway-owned metadata is bounded, generated by Praxis, and accepted
only after peer trust is established. A later design may define
whether any of that metadata is forwarded across the gateway hop.

**Site descriptors**

Praxis can consume validated local descriptions of sites,
capabilities, locality, freshness, and route metadata from locally
accepted routing snapshots.

**Capability matching**

Praxis can match request facts such as model or MCP tool metadata
against eligible local and remote capabilities. Other agent-protocol
route matching is a follow-up once the relevant request metadata is
available.

**Cross-gateway routing**

Praxis can select a local cluster or remote gateway cluster and record
the decision for logs, traces, and later filters.

**Destination-side validation**

The destination gateway re-checks peer identity and local routing
policy before forwarding to a local backend.

## Why?

### Motivation

Praxis already has strong single-gateway routing primitives:
listeners, clusters, TLS, filter chains, request metadata,
body-aware AI protocol parsing, agent protocol parsing, load
balancing, and credential isolation. Those pieces allow one
gateway to route to known local or external backends.

The next gap is gateway-to-gateway communication. Without a
first-class gateway-to-gateway model, multi-site deployments must
either expose every backend directly to every client, duplicate
routing and trust logic outside Praxis, or rely on ad hoc headers
that are difficult to secure. None of those options gives Praxis a
clear trust boundary between public clients, peer gateways, and
local backends.

Gateway-to-gateway support gives Praxis a reusable data-plane
foundation:

- a local application or agent can call its nearest Praxis gateway;
- workloads can be pointed at the platform-designated gateway and
  capability set they are allowed to use;
- that gateway can choose whether a local or remote gateway is the
  right destination;
- both gateways can use verified gateway identity, rather than
  client-controlled headers, as the basis for peer trust; and
- neither side needs to expose private backend details or secrets
  to the original caller.

This is also a prerequisite for the two demos named in the epic:
three-gateway inference routing and three-gateway agent routing.
Both demos require Praxis gateways to trust each other, record safe
route metadata, and select remote gateway clusters without
conflating the proxy data plane with a separate orchestration
system. The initial agent path can be demonstrated with MCP tool
routing; additional agent-protocol routing can follow once Praxis
has the relevant request metadata and policy semantics. The request
path should read the latest validated local routing snapshot; it
should not query the Operator, Kubernetes API, or any external state
source on every request.

### User Stories

- As a platform operator, I want one Praxis gateway to forward
  selected requests to another trusted Praxis gateway so that users
  can reach remote capabilities without direct network access to
  every backend.
- As a workload owner, I want my program to discover the gateway
  the AI platform has designated for it so that it can start using
  approved providers without direct knowledge of every backend or
  remote site.
- As a workload owner, I want my program to discover the broad
  capabilities and purpose of one assigned gateway or an aggregate
  of assigned gateways so that it can choose an appropriate
  inference or tool capability before sending traffic.
- As a security engineer, I want peer gateways authenticated with
  mTLS and mapped to known identities so that public clients cannot
  impersonate a gateway.
- As a gateway operator, I want public request metadata to be
  rejected or ignored as trusted routing input so that routing
  decisions cannot be spoofed with headers.
- As an inference platform operator, I want Praxis to choose a
  local or remote gateway based on advertised model capability and
  freshness so that requests can use available capacity outside the
  local gateway when appropriate.
- As an agent platform operator, I want Praxis to route MCP tool
  traffic across gateway boundaries using protocol metadata so that
  tools can be exposed through the same gateway trust model as
  inference.
- As an agent platform operator, I want the gateway-to-gateway model
  to support future agent-protocol routing once request metadata and
  policy semantics are available.
- As an SRE, I want route decisions to include safe, bounded
  metadata such as selected site, selected capability, and snapshot
  generation so that cross-gateway behavior can be debugged without
  logging prompts, credentials, or secrets.
- As a Praxis maintainer, I want gateway-to-gateway routing to reuse
  existing clusters, TLS, load balancing, filters, and examples so
  that the feature extends the current architecture instead of
  adding a separate proxy path.
