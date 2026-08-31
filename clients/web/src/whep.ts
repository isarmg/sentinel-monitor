function waitForIceGathering(peer: RTCPeerConnection, timeoutMs = 5_000): Promise<void> {
  if (peer.iceGatheringState === "complete") return Promise.resolve();
  return new Promise((resolve) => {
    const timeout = window.setTimeout(done, timeoutMs);
    function done() {
      window.clearTimeout(timeout);
      peer.removeEventListener("icegatheringstatechange", changed);
      resolve();
    }
    function changed() {
      if (peer.iceGatheringState === "complete") done();
    }
    peer.addEventListener("icegatheringstatechange", changed);
  });
}

function parseIceServers(header: string | null): RTCIceServer[] {
  if (header === null) return [];
  return header
    .split(/,(?=\s*<)/)
    .flatMap((entry): RTCIceServer[] => {
      const url = entry.match(/<([^>]+)>/)?.[1];
      if (url === undefined || !/rel="?ice-server"?/i.test(entry)) return [];
      const username = entry.match(/username="([^"]*)"/i)?.[1];
      const credential = entry.match(/credential="([^"]*)"/i)?.[1];
      return [{
        urls: [url],
        ...(username === undefined ? {} : { username }),
        ...(credential === undefined ? {} : { credential }),
      }];
    });
}

function resourceUrl(location: string | null, requestUrl: string): string | null {
  if (location === null || location === "") return null;
  if (/^https?:\/\//i.test(location)) return location;
  if (location.startsWith("/") && requestUrl.startsWith("/media-webrtc/")) {
    return `/media-webrtc${location}`;
  }
  return new URL(location, new URL(requestUrl, window.location.href)).toString();
}

export class WhepPlayer {
  private peer: RTCPeerConnection | null = null;
  private resource: string | null = null;

  constructor(
    private readonly video: HTMLVideoElement,
    private readonly url: string,
    private readonly token: string,
  ) {}

  async start(): Promise<void> {
    const headers = { Authorization: `Bearer ${this.token}` };
    let iceServers: RTCIceServer[] = [];
    try {
      const options = await fetch(this.url, { method: "OPTIONS", headers });
      iceServers = parseIceServers(options.headers.get("Link"));
    } catch {
      // LAN deployment can still connect by using candidates in the SDP.
    }

    const peer = new RTCPeerConnection({ iceServers });
    this.peer = peer;
    peer.addTransceiver("video", { direction: "recvonly" });
    peer.addTransceiver("audio", { direction: "recvonly" });
    peer.ontrack = (event) => {
      const stream = event.streams[0];
      if (stream !== undefined) this.video.srcObject = stream;
    };

    const connected = new Promise<void>((resolve, reject) => {
      const timeout = window.setTimeout(() => reject(new Error("WebRTC连接超时")), 12_000);
      peer.addEventListener("connectionstatechange", () => {
        if (peer.connectionState === "connected") {
          window.clearTimeout(timeout);
          resolve();
        } else if (["failed", "closed"].includes(peer.connectionState)) {
          window.clearTimeout(timeout);
          reject(new Error("WebRTC连接失败"));
        }
      });
    });

    const offer = await peer.createOffer();
    await peer.setLocalDescription(offer);
    await waitForIceGathering(peer);
    const localDescription = peer.localDescription;
    if (localDescription === null) throw new Error("浏览器未生成WHEP SDP");
    const response = await fetch(this.url, {
      method: "POST",
      headers: { ...headers, "Content-Type": "application/sdp" },
      body: localDescription.sdp,
    });
    if (!response.ok) throw new Error(`WHEP信令失败 (${response.status})`);
    this.resource = resourceUrl(response.headers.get("Location"), this.url);
    await peer.setRemoteDescription({ type: "answer", sdp: await response.text() });
    await connected;
    await this.video.play().catch(() => undefined);
  }

  close(): void {
    if (this.resource !== null) {
      void fetch(this.resource, {
        method: "DELETE",
        headers: { Authorization: `Bearer ${this.token}` },
        keepalive: true,
      }).catch(() => undefined);
    }
    this.peer?.close();
    this.peer = null;
    this.resource = null;
    this.video.srcObject = null;
  }
}
