// Newline-delimited JSON framing over a node:net socket. (The product uses
// WebSocket; this is the same JSON messages over a raw cross-process socket —
// proving the choreography over real transport between independent OS processes.)
function sendMsg(sock, obj) {
  sock.write(JSON.stringify(obj) + '\n');
}
function onMessages(sock, handler) {
  let buf = '';
  sock.on('data', (d) => {
    buf += d.toString();
    let i;
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (line.trim()) {
        let msg;
        try { msg = JSON.parse(line); } catch { continue; }
        handler(msg);
      }
    }
  });
}
module.exports = { sendMsg, onMessages };
