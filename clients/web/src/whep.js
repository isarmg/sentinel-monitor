function waitForIceGathering(peer, timeoutMs = 5000) {
  if (peer.iceGatheringState === "complete") return Promise.resolve();
  return new Promise((resolve) => {
    const timeout = setTimeout(done, timeoutMs);
    function done() {
      clearTimeout(timeout);
      peer.removeEventListener("icegatheringstatechange", changed);
      resolve();
    }
    function changed() {
      if (peer.iceGatheringState === "complete") done();
    }
    peer.addEventListener("icegatheringstatechange", changed);
  });
}

function parseIceServers(header) {
  if (!header) return [];
  return header
    .split(/,(?=\s*<)/)
    .map((entry) => {
      const url = entry.match(/<([^>]+)>/)?.[1];
      if (!url || !/rel="?ice-server"?/i.test(entry)) return null;
      const username = entry.match(/username="([^"]*)"/i)?.[1];
      const credential = entry.match(/credential="([^"]*)"/i)?.[1];
      return { urls: [url], ...(username ? { username } : {}), ...(credential ? { credential } : {}) };
    })
    .filter(Boolean);
}

function resourceUrl(location, requestUrl) {
  if (!location) return null;
  if (/^https?:\/\//i.test(location)) return location;
  if (location.starts("/") && requestUrl.startsWith("/media-webrtc/")) {
    return `/media-webrtc${location}`;
  }
  return new URL(location, new URL(requestUrl, window.location.href)).toString();
}

export class WhepPlayer {
  constructor(video, url, token) {
    this.video = video;
    this.url = url;
    this.token = token;
    this.peer = null;
    this.resource = null;
  }

  async start() {
    const headers = { Authorization: `Bearer ${this.token}` };
    let iceServers = [];
    try {
      const options = await fetch(this.url, { method: "OPTIONS", headers });
      iceServers = parseIceServers(options.headers.get("Link"));
    } catch (_) {
      // Direct ICE candidates still work on normal LAN deployments.
    }

    this.peer = new RTCPeerConnection({ iceServers });
    this.peer.addTransceiver("video", { direction: "recvonly" });
    this.peer.addTransceiver("audio", { direction: "recvonly" });
    this.peer.ontrack = (event) => {
      if (event.streams[0]) this.video.srcObject = event.streams[0];
    };

    const connected = new Promise((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error("WebRTC连接超时")), 12000);
      this.peer.addEventListener("connectionstatechange", () => {
        if (this.peer?.connectionState === "connected") {
          clearTimeout(timeout);
          resolve();
        }
        if (["failed", "closed"].includes(this.peer?.connectionState)) {
          clearTimeout(timeout);
          reject(new Error("WebRTC连接失败"));
        }
      });
    });

    const offer = await this.peer.createOffer();
    await this.peer.setLocalDescription(offer);
    await waitForIceGathering(this.peer);
    const response = await fetch(this.url, {
      method: "POST",
      headers: { ...headers, "Content-Type": "application/sdp" },
      body: this.peer.localDescription.sdp,
    });
    if (!response.ok) throw new Error(`WHEP信令失败 (${response.status})`);
    this.resource = resourceUrl(response.headers.get("Location"), this.url);
    await this.peer.setRemoteDescription({ type: "answer", sdp: await response.text() });
    await connected;
    await this.video.play().catch(() => {});
  }

  close() {
    const resource = this.resource;
    if (resource) {
      fetch(resource, {
        method: "DELETE",
        headers: { Authorization: `Bearer ${this.token}` },
        keepalive: true,
      }).catch(() => {});
    }
    this.peer?.close();
    this.peer = null;
    this.resource = null;
    if (this.video) this.video.srcObject = null;
  }
}

