// IRC bindings.

#[warn(deprecated_mode)];
#[warn(deprecated_pattern)];

extern mod std;
use dvec::{DVec};
use io::{ReaderUtil, Writer, WriterUtil};
use std::{net_ip, net_tcp};
use to_str::ToStr;
use std::uv_iotask::IoTask;
use std::net_ip::IpAddr;
use std::net_tcp::TcpSocketBuf;
use std::net_tcp::TcpConnectErrData;
use std::net_tcp::TcpErrData;
use std::net_tcp::TcpSocket;

struct UserInfo {
    username: &str,
    hostname: &str,
    servername: &str,
    realname: &str
}

/*
 * Messages
 */

enum IncomingCommand {
    PingCommand,
    PrivMsgCommand
}

type IncomingCommandTable = send_map::linear::LinearMap<~str,IncomingCommand>;

mod IncomingCommandTable {
    fn make() -> IncomingCommandTable {
        // FIXME: This is ugly due to Rust bug #3283.
        let mut map = send_map::linear::LinearMap();
        (&mut map).insert(~"PING", PingCommand);
        (&mut map).insert(~"PRIVMSG", PrivMsgCommand);
        return map;
    }
}

enum Sender {
    NoSender,
    FromSender(~str)
}

enum IncomingMsg {
    PingMsg(Sender, ~str),
    PrivInMsg(Sender, ~str, ~str),
    NumberedMsg(Sender, uint, ~[~str]),
    OtherMsg(Sender, ~str, ~[~str])
}

mod IncomingMsg {
    fn parse(commands: &IncomingCommandTable, line: &str) -> IncomingMsg {
        let mut line = line;
        let peek: &fn() -> Option<char> = || {
            if line.len() == 0 { None } else { Some(line.char_at(0)) }
        };
        let next: &fn() -> Option<char> = || {
            if line.len() == 0 {
                None
            } else {
                let (ch, tmp) = str::view_shift_char(line);
                line = tmp;
                Some(ch)
            }
        };
        let get_word: &fn() -> ~str = || {
            let mut buf = ~"";
            loop {
                match next() {
                    None | Some(' ') | Some('\r') | Some('\n') => break,
                    Some(ch) => str::push_char(buf, ch)
                }
            }
            buf
        };

        // Parse the sender.
        debug!("parsed sender");
        let sender;
        if peek() == Some(':') {
            sender = FromSender(get_word());
        } else {
            sender = NoSender;
        }

        // Parse the command.
        debug!("parsed command");
        let command = get_word();

        // Parse any arguments.
        let args = DVec();
        loop {
            match peek() {
                None => break,
                Some('\r') | Some('\n') => {
                    next();
                    break;
                }
                Some(':') => {
                    // Eat the rest of the line.
                    next();
                    let mut rest = line.to_str();
                    // FIXME: This is rather inefficient.
                    if rest.ends_with("\r") {
                        str::pop_char(rest);
                    }
                    args.push(move rest);
                    break;
                }
                Some(_) => {
                    args.push(get_word());
                }
            }
            debug!("parsed arg");
        }

        // Figure out the command.
        match commands.find(&command) {
            Some(PingCommand) if args.len() >= 1    => PingMsg(sender, args[0]),
            Some(PrivMsgCommand) if args.len() >= 2 => PrivInMsg(sender, args[0], args[1]),
            Some(PingCommand) | Some(PrivMsgCommand) | None => {
                match uint::from_str(command) {
                    Some(index) => NumberedMsg(sender, index, vec::from_mut(dvec::unwrap(args))),
                    None => OtherMsg(sender, command, vec::from_mut(dvec::unwrap(args)))
                }
            }
        }
    }
}

enum OutgoingMsg {
    PongMsg(&str),
    PassMsg(&str),
    NickMsg(&str),
    UserMsg([ &str * 4 ]),
    JoinMsg(&str, Option<&str>),
    PartMsg(&str),
    PrivOutMsg(&str, &str)
}

impl OutgoingMsg {
    // Iterates over each message argument.
    fn eachi(&self, f: &fn(uint, s: &str) -> bool) {
        match *self {
            PongMsg(s) | PassMsg(s) | NickMsg(s) | PartMsg(s) => { f(0, s); }
            JoinMsg(s, None) => { f(0, s); }
            JoinMsg(s0, Some(s1)) | PrivOutMsg(s0, s1) => { f(0, s0); f(1, s1); }
            UserMsg(ref ss) => { ss.eachi(|i, s| f(i, s)); }
        }
    }

    // Returns the number of arguments.
    fn arg_count(&self) -> uint {
        match *self {
            PongMsg(_) | PassMsg(_) | NickMsg(_) | JoinMsg(_, None) | PartMsg(_) => 1,
            JoinMsg(_, Some(_)) | PrivOutMsg(_, _) => 2,
            UserMsg(*) => 4
        }
    }

    // Returns the token that identifies this message.
    fn token(&self) -> &static/str {
        match *self {
            PongMsg(*)    => "PONG",
            PassMsg(*)    => "PASS",
            NickMsg(*)    => "NICK",
            UserMsg(*)    => "USER",
            JoinMsg(*)    => "JOIN",
            PartMsg(*)    => "PART",
            PrivOutMsg(*) => "PRIVMSG"
        }
    }

    // Returns true if this message needs a colon before the last argument.
    fn needs_colon(&self) -> bool {
        match *self {
            PongMsg(*) | PassMsg(*) | NickMsg(*) | JoinMsg(*) | PartMsg(*) => false,
            UserMsg(*) | PrivOutMsg(*) => true
        }
    }

    // Writes this message to a stream, and flushes it.
    fn write<W:Writer>(&self, out: &W) {
        (*out).write_str(self.token());

        let arg_count = self.arg_count();
        for self.eachi |i, arg| {
            (*out).write_char(' ');

            if i == arg_count - 1 {     // Are we the last message?
                if self.needs_colon() || arg.contains_char(' ') {
                    (*out).write_char(':');
                }
            } else {
                assert !arg.contains_char(' ');
            }

            (*out).write_str(arg);
        }

        (*out).write_char('\n');
        (*out).flush();
    }
}

/*
 * Connections
 */

struct Connection {
    iotask: IoTask,
    socket: TcpSocketBuf,
    commands: IncomingCommandTable
}

mod Connection {
    fn make(+server_ip: IpAddr, port: uint, nick: &str, user: &UserInfo, password: &str,
            +iotask: IoTask)
         -> Result<Connection,TcpConnectErrData> {
        let self;
        match move net_tcp::connect(server_ip, port, iotask) {
            Ok(move socket) => {
                let socket = net_tcp::socket_buf(socket);
                let commands = IncomingCommandTable::make();
                self = Connection {
                    iotask: iotask,
                    socket: move socket,
                    commands: move commands
                };
            }
            Err(move e) => return Err(e)
        }

        self.send(PassMsg(password));
        self.send(NickMsg(nick));
        self.send(UserMsg([ user.username, user.hostname, user.servername, user.realname ]));

        return Ok(self);
    }
}

impl Connection {
    // Receives a message and returns it. Pings are automatically responded to.
    fn recv(&self) -> IncomingMsg {
        let msg = IncomingMsg::parse(&self.commands, self.socket.read_line());
        match msg {
            PingMsg(_, ref payload) => self.send(PongMsg(*payload)),
            _ => {}
        }
        return msg;
    }

    // Sends an IRC message.
    fn send(&self, +msg: OutgoingMsg) {
        msg.write(&self.socket);
    }
}

// FIXME: Workaround for assignability restrictions (Rust issue #3285).
fn id(s: &a/str) -> &a/str { s }

pure fn get_ref<T:Copy,U>(r: &a/Result<T,U>) -> &a/T {
    match *r {
        Ok(ref r) => r,
        Err(ref the_err) => unchecked {
            fail fmt!("get called on error result: %?", *the_err)
        }
    }
}

fn main(args: ~[~str]) {
    let (server, username, channel) = (copy args[1], copy args[2], copy args[3]);
    io::println(fmt!("%s %s %s", server, username, channel));
    let iotask = std::uv_global_loop::get();
    let addr = copy result::unwrap(net_ip::get_addr(server, iotask))[0];
    let userinfo = UserInfo {
        username: id(username), // FIXME: Needs assignability here (Rust issue #3285).
        hostname: "asdf.com",
        servername: "localhost",
        realname: "Robots are friendly"
    };
    let conn_result = Connection::make(addr, 6667, username, &userinfo, "x", iotask);
    let conn = get_ref(&conn_result);
    conn.send(JoinMsg(channel, None));
    loop {
        io::println(fmt!("%?", conn.recv()));
    }
}

