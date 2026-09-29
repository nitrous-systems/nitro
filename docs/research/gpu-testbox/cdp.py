import json,urllib.request,socket,base64,os,struct
v=json.load(urllib.request.urlopen("http://127.0.0.1:9333/json/version"))
u=v["webSocketDebuggerUrl"].split("//",1)[1]; hp,path=u.split("/",1); h,p=hp.split(":")
s=socket.create_connection((h,int(p))); k=base64.b64encode(os.urandom(16)).decode()
s.send(f"GET /{path} HTTP/1.1\r\nHost: {hp}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {k}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode())
buf=b""
while b"\r\n\r\n" not in buf: buf+=s.recv(4096)
buf=buf.split(b"\r\n\r\n",1)[1]
msg=json.dumps({"id":1,"method":"SystemInfo.getInfo"}).encode(); m=os.urandom(4)
hdr=bytes([0x81])+(bytes([0x80|len(msg)]) if len(msg)<126 else bytes([0x80|126])+struct.pack(">H",len(msg)))
s.send(hdr+m+bytes(b^m[i%4] for i,b in enumerate(msg)))
def rd(n):
    global buf
    while len(buf)<n: buf+=s.recv(65536)
    r,buf=buf[:n],buf[n:]; return r
while True:
    b0,b1=rd(2); n=b1&127
    if n==126: n=struct.unpack(">H",rd(2))[0]
    elif n==127: n=struct.unpack(">Q",rd(8))[0]
    d=json.loads(rd(n))
    if d.get("id")==1: break
g=d["result"]["gpu"]; a=g.get("auxAttributes",{})
for key in ["glRenderer","glVersion","glImplementationParts","displayType","skiaBackendType","hardwareSupportsVulkan"]:
    if key in a: print(key,"=",a[key])
for dv in g["devices"]: print("device",hex(int(dv["vendorId"])),hex(int(dv["deviceId"])),dv.get("driverVendor"),dv.get("driverVersion"))
print("featureStatus",json.dumps(g.get("featureStatus")))
