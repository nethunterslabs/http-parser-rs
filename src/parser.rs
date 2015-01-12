use std::u64;
use std::cmp;

use state::{State, HeaderState};
use flags::Flags;
use error::HttpErrno;
use http_method::HttpMethod;
use http_version::HttpVersion;
use callback::{HttpParserCallback, CallbackDecision, CallbackResult};

#[derive(PartialEq, Eq, Copy)]
pub enum HttpParserType {
    Request,
    Response,
    Both,
}

pub struct HttpParser {
    pub http_version: HttpVersion,
    pub errno : Option<HttpErrno>,
    pub status_code : u16,          // response only
    pub method : HttpMethod,        // request only

    pub upgrade : bool,
    
    pub strict : bool,      // parsing using strict rules

    // private
    tp : HttpParserType,
    state : State,
    header_state : HeaderState,
    flags : u8,
    index : usize,             // index into current matcher

    nread : usize,            // bytes read in various scenarios
    content_length : u64,   // bytes in body (0 if no Content-Length header)
}

//============== End of public interfaces ===================

macro_rules! ensure_error(
    ($parser:ident) => (
        if $parser.errno.is_none() {
            $parser.errno = Option::Some(HttpErrno::Unknown);
        }
    );
);

macro_rules! assert_ok(
    ($parser:ident) => (
        assert!($parser.errno.is_none());
    );
);

macro_rules! callback(
    ($parser:ident, $cb:expr, $err:expr) => (
       match $cb {
           Err(..) => $parser.errno = Option::Some($err),
           _ => (),
       }
    );
);

macro_rules! callback_data(
    ($parser:ident, $mark:ident, $cb:expr, $err:expr, $idx:expr) => (
        if $mark.is_some() {
            match $cb {
                Err(..) => $parser.errno = Option::Some($err),
                _ => (),
            }

            if $parser.errno.is_some() {
                return $idx;
            }
            // Necessary to reset mark, though it causes unused warning
            $mark = None;
        }
    );
);

macro_rules! start_state(
    ($parser:ident) => (
        if $parser.tp == HttpParserType::Request {
            State::StartReq
        } else {
            State::StartRes
        }
    );
);

macro_rules! strict_check(
    ($parser:ident, $cond:expr, $idx:expr) => (
        if $parser.strict && $cond {
            $parser.errno = Option::Some(HttpErrno::Strict);
            return $idx;
        }
    );
);

macro_rules! new_message(
    ($parser:ident) => (
        if $parser.strict {
            if $parser.http_should_keep_alive() {
                start_state!($parser)
            } else {
                State::Dead
            }
        } else {
            start_state!($parser)
        }
    );
);

macro_rules! mark(
    ($mark:ident, $idx:expr) => (
        if $mark.is_none() {
            $mark = Some($idx);
        }
    );
);

const HTTP_MAX_HEADER_SIZE : usize = 80*1024;
const ULLONG_MAX : u64 = u64::MAX - 1;

const CR : u8 = b'\r';
const LF : u8 = b'\n';

const PROXY_CONNECTION : &'static str = "proxy-connection";
const CONNECTION : &'static str = "connection";
const CONTENT_LENGTH : &'static str = "content-length";
const TRANSFER_ENCODING : &'static str = "transfer-encoding";
const UPGRADE : &'static str = "upgrade";
const CHUNKED : &'static str = "chunked";
const KEEP_ALIVE : &'static str = "keep-alive";
const CLOSE : &'static str = "close";

const TOKEN : [Option<u8>; 256] = [
    //   0 nul      1 soh       2 stx       3 etx      4 eot        5 enq       6 ack       7 bel   
         None,       None,     None,        None,       None,       None,        None,      None,       
    //   8 bs        9 ht      10 nl        11 vt      12 np        13 cr       14 so       15 si    
         None,       None,     None,        None,       None,       None,        None,      None,       
    //  16 dle     17 dc1      18 dc2       19 dc3     20 dc4       21 nak      22 syn      23 etb    
         None,       None,     None,        None,       None,       None,        None,      None,       
    //  24 can     25 em       26 sub       27 esc     28 fs        29 gs       30 rs       31 us    
         None,       None,     None,        None,       None,       None,        None,      None,       
    //  32 sp      33   !      34  "        35  #      36   $       37  %       38  &       39   '    
         None, Some(b'!'),     None,  Some(b'#'), Some(b'$'),  Some(b'%'), Some(b'&'), Some(b'\''),      
    //  40  (      41  )       42  *        43  +      44  ,        45  -       46  .       47  /    
         None,     None, Some(b'*'),  Some(b'+'),      None,  Some(b'-'), Some(b'.'),       None,       
    //  48  0      49  1       50  2        51  3      52  4        53  5       54  6       55  7    
       Some(b'0'), Some(b'1'), Some(b'2'), Some(b'3'), Some(b'4'), Some(b'5'), Some(b'6'), Some(b'7'),      
    //  56  8      57  9       58  :        59  ;      60  <        61  =       62  >       63  ?    
       Some(b'8'), Some(b'9'), None,        None,      None,        None,       None,       None,       
    //  64  @      65  A       66  B        67  C      68  D        69  E       70  F       71  G    
        None, Some(b'a'), Some(b'b'), Some(b'c'), Some(b'd'), Some(b'e'), Some(b'f'), Some(b'g'),      
    //  72  H      73  I       74  J        75  K      76  L        77  M       78  N       79  O    
       Some(b'h'), Some(b'i'), Some(b'j'), Some(b'k'), Some(b'l'), Some(b'm'), Some(b'n'), Some(b'o'),      
    //  80  P      81  Q       82  R        83  S      84  T        85  U       86  V       87  W    
       Some(b'p'), Some(b'q'), Some(b'r'), Some(b's'), Some(b't'), Some(b'u'), Some(b'v'), Some(b'w'),      
    //  88  X      89  Y       90  Z        91  [      92  \        93  ]       94  ^       95  _    
       Some(b'x'), Some(b'y'), Some(b'z'),  None,      None,        None,      Some(b'^'), Some(b'_'),      
    //  96  `      97  a       98  b        99  c      100  d       101  e      102  f      103  g    
       Some(b'`'), Some(b'a'), Some(b'b'),  Some(b'c'), Some(b'd'), Some(b'e'), Some(b'f'), Some(b'g'),      
    // 104  h      105  i      106  j       107  k     108  l       109  m      110  n      111  o    
       Some(b'h'), Some(b'i'), Some(b'j'),  Some(b'k'), Some(b'l'), Some(b'm'), Some(b'n'), Some(b'o'),      
    // 112  p      113  q      114  r       115  s     116  t       117  u      118  v      119  w    
       Some(b'p'), Some(b'q'), Some(b'r'),  Some(b's'), Some(b't'), Some(b'u'), Some(b'v'), Some(b'w'),      
    // 120  x      121  y      122  z       123  {     124  |       125  }      126  ~      127 del    
       Some(b'x'), Some(b'y'), Some(b'z'),  None,       Some(b'|'), None,       Some(b'~'), None,
    // no one is token afterwards
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None,       
        None,       None,     None,        None,       None,       None,        None,      None];

const NORMAL_URL_CHAR : [u8; 32] = [
    //   0 nul    1 soh    2 stx    3 etx    4 eot    5 enq    6 ack    7 bel   
            0    |   0    |   0    |   0    |   0    |   0    |   0    |   0,       
    //   8 bs     9 ht    10 nl    11 vt    12 np    13 cr    14 so    15 si
            0    |   2    |   0    |   0    |   16   |   0    |   0    |   0, // TODO add T()
    //  16 dle   17 dc1   18 dc2   19 dc3   20 dc4   21 nak   22 syn   23 etb
            0    |   0    |   0    |   0    |   0    |   0    |   0    |   0,       
    //  24 can   25 em    26 sub   27 esc   28 fs    29 gs    30 rs    31 us    
            0    |   0    |   0    |   0    |   0    |   0    |   0    |   0,       
    //  32 sp    33  !    34  "    35  #    36  $    37  %    38  &    39  '    
            0    |   2    |   4    |   0    |   16   |   32   |   64   |  128,      
    //  40  (    41  )    42  *    43  +    44  ,    45  -    46  .    47  /    
            1    |   2    |   4    |   8    |   16   |   32   |   64   |  128,      
    //  48  0    49  1    50  2    51  3    52  4    53  5    54  6    55  7    
            1    |   2    |   4    |   8    |   16   |   32   |   64   |  128,      
    //  56  8    57  9    58  :    59  ;    60  <    61  =    62  >    63  ?    
            1    |   2    |   4    |   8    |   16   |   32   |   64   |   0,       
    //  64  @    65  A    66  B    67  C    68  D    69  E    70  F    71  G    
            1    |   2    |   4    |   8    |   16   |   32   |   64   |  128,      
    //  72  H    73  I    74  J    75  K    76  L    77  M    78  N    79  O    
            1    |   2    |   4    |   8    |   16   |   32   |   64   |  128,      
    //  80  P    81  Q    82  R    83  S    84  T    85  U    86  V    87  W    
            1    |   2    |   4    |   8    |   16   |   32   |   64   |  128,      
    //  88  X    89  Y    90  Z    91  [    92  \    93  ]    94  ^    95  _    
            1    |   2    |   4    |   8    |   16   |   32   |   64   |  128,      
    //  96  `    97  a    98  b    99  c   100  d   101  e   102  f   103  g    
            1    |   2    |   4    |   8    |   16   |   32   |   64   |  128,      
    // 104  h   105  i   106  j   107  k   108  l   109  m   110  n   111  o   
            1    |   2    |   4    |   8    |   16   |   32   |   64   |  128,      
    // 112  p   113  q   114  r   115  s   116  t   117  u   118  v   119  w   
            1    |   2    |   4    |   8    |   16   |   32   |   64   |  128,      
    // 120  x   121  y   122  z   123  {   124  |   125  }   126  ~   127 del
            1    |   2    |   4    |   8    |   16   |   32   |   64   |   0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

const UNHEX : [i8; 256] = [
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
     0, 1, 2, 3, 4, 5, 6, 7, 8, 9,-1,-1,-1,-1,-1,-1,
    -1,10,11,12,13,14,15,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,10,11,12,13,14,15,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,
    -1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1,-1];

// TODO replace some functions by macros

fn token(hp : &HttpParser, ch :u8) -> Option<u8> {
    if hp.strict {
        TOKEN[ch as usize]
    } else {
        if ch == b' ' { Some(b' ') } else { TOKEN[ch as usize] }
    }
}

fn is_url_char(hp : &HttpParser, ch : u8) -> bool {
    let res = (NORMAL_URL_CHAR[(ch >> 3) as usize] & (1 << ((ch & 7) as usize))) != 0;
    res || (!hp.strict && (ch & 0x80) > 0)
}

fn lower(ch : u8) -> u8 {
    ch | 0x20
}

fn is_num(ch : u8) -> bool {
    ch >= b'0' && ch <= b'9'
}

fn is_alpha(ch : u8) -> bool {
    (ch >= b'a' && ch <= b'z') || (ch >= b'A' && ch <= b'Z')
}

fn is_alphanum(ch : u8) -> bool {
    is_num(ch) || is_alpha(ch)
}

fn is_mark(ch : u8) -> bool {
    ch == b'-' || ch == b'_' || ch == b'.' || ch == b'!' || ch == b'~' || 
        ch == b'*' || ch == b'\'' || ch == b'(' || ch == b')'
}

fn is_userinfo_char(ch : u8) -> bool {
    is_alphanum(ch) || is_mark(ch) || ch == b'%' || 
        ch == b';' || ch == b':' || ch == b'&' || ch == b'=' || 
        ch == b'+' || ch == b'$' || ch == b','
}

impl HttpParser {
    pub fn new(tp : HttpParserType) -> HttpParser {
        HttpParser { 
            tp : tp,  
            state : match tp {
                        HttpParserType::Request     => State::StartReq,
                        HttpParserType::Response    => State::StartRes,
                        HttpParserType::Both        => State::StartReqOrRes,
                    },
            header_state : HeaderState::General,
            flags : 0,
            index : 0,
            nread : 0,
            content_length: ULLONG_MAX,
            http_version: HttpVersion { major: 1, minor: 0 },
            errno : Option::None,
            status_code : 0,
            method : HttpMethod::Get,
            upgrade : false,
            strict: true,
        }
    }

    pub fn execute<T: HttpParserCallback>(&mut self, cb : &mut T, data : &[u8]) -> u64 {
        let mut index : u64 = 0;
        let len : u64 = data.len() as u64;
        let mut header_field_mark : Option<u64> = None;
        let mut header_value_mark : Option<u64> = None;
        let mut url_mark : Option<u64> = None;
        let mut body_mark : Option<u64> = None;
        let mut status_mark : Option<u64> = None;

        if self.errno.is_some() {
            return 0;
        }

        if len == 0 {    // mean EOF
            match self.state {
                State::BodyIdentityEof => {
                    assert_ok!(self);
                    callback!(self, cb.on_message_complete(self), 
                              HttpErrno::CBMessageComplete);
                    if self.errno.is_some() {
                        return index;
                    }
                    return 0;
                },
                State::Dead | 
                State::StartReqOrRes | 
                State::StartReq | 
                State::StartRes => {
                    return 0;
                }
                _ => {
                   self.errno = Option::Some(HttpErrno::InvalidEofState);
                   return 1;
                }
            }
        }

        if self.state == State::HeaderField {
            header_field_mark = Some(0);
        }
        if self.state == State::HeaderValue {
            header_value_mark = Some(0);
        }
        match self.state {
            State::ReqPath |
            State::ReqSchema |
            State::ReqSchemaSlash |
            State::ReqSchemaSlashSlash |
            State::ReqServerStart |
            State::ReqServer |
            State::ReqServerWithAt |
            State::ReqQueryStringStart |
            State::ReqQueryString |
            State::ReqFragmentStart |
            State::ReqFragment => url_mark = Some(0),
            State::ResStatus => status_mark = Some(0),
            _ => (),
        }

        while index < len {
            let ch = data[index as usize];
            if self.state <= State::HeadersDone {
                self.nread += 1;

                // From http_parser.c

                // Don't allow the total size of the HTTP headers (including the status
                // line) to exceed HTTP_MAX_HEADER_SIZE. This check is here to protect
                // embedders against denial-of-service attacks where the attacker feeds
                // us a never-ending header that the embedder keeps buffering.
                // 
                // This check is arguably the responsibility of embedders but we're doing
                // it on the embedder's behalf because most won't bother and this way we
                // make the web a little safer. HTTP_MAX_HEADER_SIZE is still far bigger
                // than any reasonable request or response so this should never affect
                // day-to-day operation.
                if self.nread > HTTP_MAX_HEADER_SIZE {
                    self.errno = Option::Some(HttpErrno::HeaderOverflow);
                    return index;
                }
            }

            // using loop to mimic 'goto reexecute_byte' in http_parser.c
            let mut retry = false;
            loop {
                retry = false;  // reset in each loop
                match self.state {
                    State::Dead => {
                        if ch != CR && ch != LF {
                            self.errno = Option::Some(HttpErrno::ClosedConnection);
                            return index;
                        }
                    },
                    State::StartReqOrRes => {
                        if ch != CR && ch != LF {
                            self.flags = 0;
                            self.content_length = ULLONG_MAX;

                            if ch == b'H' {
                                self.state = State::ResOrRespH;
                                assert_ok!(self);
                                callback!(self, cb.on_message_begin(self),
                                    HttpErrno::CBMessageBegin);
                                if self.errno.is_some() {
                                    return index+1;
                                }
                            } else {
                                self.tp = HttpParserType::Request;
                                self.state = State::StartReq;
                                retry = true;
                            }
                        }
                    },
                    State::ResOrRespH => {
                        if ch == b'T' {
                            self.tp = HttpParserType::Response;
                            self.state = State::ResHT;
                        } else {
                            if ch != b'E' {
                                self.errno = Option::Some(HttpErrno::InvalidConstant);
                                return index;
                            }

                            self.tp = HttpParserType::Request;
                            self.method = HttpMethod::Head;
                            self.index = 2;
                            self.state = State::ReqMethod;
                        }
                    },
                    State::StartRes => {
                        self.flags = 0;
                        self.content_length = ULLONG_MAX;

                        match ch {
                            b'H' => self.state = State::ResH,
                            CR | LF => (),
                            _ => {
                                self.errno = Option::Some(HttpErrno::InvalidConstant);
                                return index;
                            },
                        }
                        
                        assert_ok!(self);
                        callback!(self, cb.on_message_begin(self), 
                                  HttpErrno::CBMessageBegin);
                        if self.errno.is_some() {
                            return index+1;
                        }
                    },
                    State::ResH => {
                        strict_check!(self, ch != b'T', index);                       
                        self.state = State::ResHT;
                    },
                    State::ResHT => {
                        strict_check!(self, ch != b'T', index);
                        self.state = State::ResHTT;
                    },
                    State::ResHTT => {
                        strict_check!(self, ch != b'P', index);
                        self.state = State::ResHTTP;
                    },
                    State::ResHTTP => {
                        strict_check!(self, ch != b'/', index);
                        self.state = State::ResFirstHttpMajor;
                    },
                    State::ResFirstHttpMajor => {
                        if ch < b'0' || ch > b'9' {
                            self.errno = Option::Some(HttpErrno::InvalidVersion);
                            return index;
                        }
                        self.http_version.major = ch - b'0';
                        self.state = State::ResHttpMajor;
                    },
                    State::ResHttpMajor => {
                        if ch == b'.' {
                            self.state = State::ResFirstHttpMinor;
                        } else {
                            if !is_num(ch) {
                                self.errno = Option::Some(HttpErrno::InvalidVersion);
                                return index;
                            }

                            self.http_version.major *= 10;
                            self.http_version.major += ch - b'0';

                            if self.http_version.major > 99 {
                                self.errno = Option::Some(HttpErrno::InvalidVersion);
                                return index;
                            }
                        }
                    },
                    State::ResFirstHttpMinor => {
                        if !is_num(ch) {
                            self.errno = Option::Some(HttpErrno::InvalidVersion);
                            return index;
                        }

                        self.http_version.minor = ch - b'0';
                        self.state = State::ResHttpMinor;
                    },
                    // minor HTTP version or end of request line
                    State::ResHttpMinor => {
                        if ch == b' ' {
                            self.state = State::ResFirstStatusCode;
                        } else {
                            if !is_num(ch) {
                                self.errno = Option::Some(HttpErrno::InvalidVersion);
                                return index;
                            }

                            self.http_version.minor *= 10;
                            self.http_version.minor += ch - b'0';

                            if self.http_version.minor > 99 {
                                self.errno = Option::Some(HttpErrno::InvalidVersion);
                                return index;
                            }
                        }
                    },
                    State::ResFirstStatusCode => {
                        if !is_num(ch) {
                            if ch != b' ' {
                                self.errno = Option::Some(HttpErrno::InvalidStatus);
                                return index;
                            }
                        } else {
                            self.status_code = (ch - b'0') as u16;
                            self.state = State::ResStatusCode;
                        }
                    },
                    State::ResStatusCode => {
                        if !is_num(ch) {
                            match ch {
                                b' ' => self.state = State::ResStatusStart,
                                CR   => self.state = State::ResLineAlmostDone,
                                LF   => self.state = State::HeaderFieldStart,
                                _    => {
                                    self.errno = Option::Some(HttpErrno::InvalidStatus);
                                    return index;
                                }
                            }
                        } else {
                            self.status_code *= 10;
                            self.status_code += (ch - b'0') as u16;

                            if self.status_code > 999 {
                                self.errno = Option::Some(HttpErrno::InvalidStatus);
                                return index;
                            }
                        }
                    },
                    State::ResStatusStart => {
                        if ch == CR {
                            self.state = State::ResLineAlmostDone;
                        } else if ch == LF {
                            self.state = State::HeaderFieldStart;
                        } else {
                            mark!(status_mark, index);
                            self.state = State::ResStatus;
                            self.index = 0;
                        }
                    },
                    State::ResStatus => {
                        if ch == CR {
                            self.state = State::ResLineAlmostDone;
                            assert_ok!(self);
                            callback_data!(self, status_mark,
                                cb.on_status(self, data.slice(status_mark.unwrap() as usize, index as usize)),
                                HttpErrno::CBStatus, index+1);
                        } else if ch == LF {
                            self.state = State::HeaderFieldStart;
                            assert_ok!(self);
                            callback_data!(self, status_mark,
                                cb.on_status(self, data.slice(status_mark.unwrap() as usize, index as usize)),
                                HttpErrno::CBStatus, index+1);
                        }
                    },
                    State::ResLineAlmostDone => {
                        strict_check!(self, ch != LF, index);
                        self.state = State::HeaderFieldStart;
                    },
                    State::StartReq => {
                        if ch != CR && ch != LF {
                            self.flags = 0;
                            self.content_length = ULLONG_MAX;

                            if !is_alpha(ch) {
                                self.errno = Option::Some(HttpErrno::InvalidMethod);
                                return index;
                            }

                            self.method = HttpMethod::Delete;
                            self.index = 1;
                            match ch {
                                b'C' => self.method = HttpMethod::Connect, // or Copy, Checkout
                                b'D' => self.method = HttpMethod::Delete,
                                b'G' => self.method = HttpMethod::Get,
                                b'H' => self.method = HttpMethod::Head,
                                b'L' => self.method = HttpMethod::Lock,
                                b'M' => self.method = HttpMethod::MKCol, // or Move, MKActivity, Merge, MSearch, MKCalendar
                                b'N' => self.method = HttpMethod::Notify,
                                b'O' => self.method = HttpMethod::Options,
                                b'P' => self.method = HttpMethod::Post, // or PropFind|PropPatch|Put|Patch|Purge
                                b'R' => self.method = HttpMethod::Report,
                                b'S' => self.method = HttpMethod::Subscribe, // or Search
                                b'T' => self.method = HttpMethod::Trace,
                                b'U' => self.method = HttpMethod::Unlock, // or Unsubscribe
                                _ => {
                                    self.errno = Option::Some(HttpErrno::InvalidMethod);
                                    return index;
                                },
                            }
                            self.state = State::ReqMethod;

                            assert_ok!(self);
                            callback!(self, cb.on_message_begin(self), 
                                      HttpErrno::CBMessageBegin);
                            if self.errno.is_some() {
                                return index+1;
                            }
                        }
                    },
                    State::ReqMethod => {
                        if index == len {
                            self.errno = Option::Some(HttpErrno::InvalidMethod);
                            return index;
                        }

                        let matcher_string = self.method.to_string();
                        let matcher = matcher_string.as_slice();
                        if ch == b' ' && self.index == matcher.len() {
                            self.state = State::ReqSpacesBeforeUrl;
                        } else if self.index < matcher.len() && ch == (matcher.char_at(self.index) as u8) {
                            ;
                        } else if self.method == HttpMethod::Connect {
                            if self.index == 1 && ch == b'H' {
                                self.method = HttpMethod::Checkout;
                            } else if self.index == 2 && ch == b'P' {
                                self.method = HttpMethod::Copy;
                            } else {
                                self.errno = Option::Some(HttpErrno::InvalidMethod);
                                return index;
                            }
                        } else if self.method == HttpMethod::MKCol {
                            if self.index == 1 && ch == b'O' {
                                self.method = HttpMethod::Move;
                            } else if self.index == 1 && ch == b'E' {
                                self.method = HttpMethod::Merge;
                            } else if self.index == 1 && ch == b'-' {
                                self.method = HttpMethod::MSearch;
                            } else if self.index == 2 && ch == b'A' {
                                self.method = HttpMethod::MKActivity;
                            } else if self.index == 3 && ch == b'A' {
                                self.method = HttpMethod::MKCalendar;
                            } else {
                                self.errno = Option::Some(HttpErrno::InvalidMethod);
                                return index;
                            }
                        } else if self.method == HttpMethod::Subscribe {
                            if self.index == 1 && ch == b'E' {
                                self.method = HttpMethod::Search;
                            } else {
                                self.errno = Option::Some(HttpErrno::InvalidMethod);
                                return index;
                            }
                        } else if self.index == 1 && self.method == HttpMethod::Post {
                           if ch == b'R' {
                               self.method = HttpMethod::PropFind; // or PropPatch
                           } else if ch == b'U' {
                               self.method = HttpMethod::Put; // or Purge
                           } else if ch == b'A' {
                               self.method = HttpMethod::Patch;
                           } else {
                               self.errno = Option::Some(HttpErrno::InvalidMethod);
                               return index;
                           }
                        } else if self.index == 2 {
                            if self.method == HttpMethod::Put {
                                if ch == b'R' {
                                    self.method = HttpMethod::Purge;
                                } else {
                                    self.errno = Option::Some(HttpErrno::InvalidMethod);
                                    return index;
                                }
                            } else if self.method == HttpMethod::Unlock {
                                if ch == b'S' {
                                    self.method = HttpMethod::Unsubscribe;
                                } else {
                                    self.errno = Option::Some(HttpErrno::InvalidMethod);
                                    return index;
                                }
                            } else {
                                self.errno = Option::Some(HttpErrno::InvalidMethod);
                                return index;
                            }
                        } else if self.index == 4 && self.method == HttpMethod::PropFind && ch == b'P' {
                            self.method = HttpMethod::PropPatch;
                        } else {
                            self.errno = Option::Some(HttpErrno::InvalidMethod);
                            return index;
                        }

                        self.index += 1;
                    },
                    State::ReqSpacesBeforeUrl => {
                        if ch != b' ' {
                            mark!(url_mark, index);
                            if self.method == HttpMethod::Connect {
                                self.state = State::ReqServerStart;
                            }

                            self.state = HttpParser::parse_url_char(self, self.state, ch);
                            if self.state == State::Dead {
                                self.errno = Option::Some(HttpErrno::InvalidUrl);
                                return index;
                            }
                        }
                    },
                    State::ReqSchema |
                    State::ReqSchemaSlash |
                    State::ReqSchemaSlashSlash |
                    State::ReqServerStart => {
                        match ch {
                            // No whitespace allowed here
                            b' ' | CR | LF => {
                                self.errno = Option::Some(HttpErrno::InvalidUrl);
                                return index;
                            },
                            _ => {
                                self.state = HttpParser::parse_url_char(self, self.state, ch);
                                if self.state == State::Dead {
                                    self.errno = Option::Some(HttpErrno::InvalidUrl);
                                    return index;
                                }
                            }
                        }
                    },
                    State::ReqServer |
                    State::ReqServerWithAt |
                    State::ReqPath |
                    State::ReqQueryStringStart |
                    State::ReqQueryString |
                    State::ReqFragmentStart |
                    State::ReqFragment => {
                        match ch {
                            b' ' => {
                                self.state = State::ReqHttpStart;
                                assert_ok!(self);
                                callback_data!(self, url_mark,
                                    cb.on_url(self, data.slice(url_mark.unwrap() as usize, index as usize)),
                                    HttpErrno::CBUrl, index+1);
                            },
                            CR | LF => {
                                self.http_version.major = 0;
                                self.http_version.minor = 9;
                                self.state = if ch == CR {
                                    State::ReqLineAlmostDone 
                                } else {
                                    State::HeaderFieldStart
                                };
                                assert_ok!(self);
                                callback_data!(self, url_mark,
                                    cb.on_url(self, data.slice(url_mark.unwrap() as usize, index as usize)),
                                    HttpErrno::CBUrl, index+1);
                            },
                            _ => {
                                self.state = HttpParser::parse_url_char(self, self.state, ch);
                                if self.state == State::Dead {
                                    self.errno = Option::Some(HttpErrno::InvalidUrl);
                                    return index;
                                }
                            }
                        }
                    },
                    State::ReqHttpStart => {
                        match ch {
                            b'H' => self.state = State::ReqHttpH,
                            b' ' => (),
                            _    => {
                                self.errno = Option::Some(HttpErrno::InvalidConstant);
                                return index;
                            }
                        }
                    },
                    State::ReqHttpH => {
                        strict_check!(self, ch != b'T', index);
                        self.state = State::ReqHttpHT;
                    },
                    State::ReqHttpHT => {
                        strict_check!(self, ch != b'T', index);
                        self.state = State::ReqHttpHTT;
                    },
                    State::ReqHttpHTT => {
                        strict_check!(self, ch != b'P', index);
                        self.state = State::ReqHttpHTTP;
                    },
                    State::ReqHttpHTTP => {
                        strict_check!(self, ch != b'/', index);
                        self.state = State::ReqFirstHttpMajor;
                    },
                    // first digit of major HTTP version
                    State::ReqFirstHttpMajor => {
                        if ch < b'1' || ch > b'9' {
                            self.errno = Option::Some(HttpErrno::InvalidVersion);
                            return index;
                        }

                        self.http_version.major = ch - b'0';
                        self.state = State::ReqHttpMajor;
                    },
                    // major HTTP version or dot
                    State::ReqHttpMajor => {
                        if ch == b'.' {
                            self.state = State::ReqFirstHttpMinor;
                        } else {
                            if !is_num(ch) {
                                self.errno = Option::Some(HttpErrno::InvalidVersion);
                                return index;
                            }

                            self.http_version.major *= 10;
                            self.http_version.major += ch - b'0';

                            if self.http_version.major > 99 {
                                self.errno = Option::Some(HttpErrno::InvalidVersion);
                                return index;
                            }
                        }
                    },
                    // first digit of minor HTTP version
                    State::ReqFirstHttpMinor => {
                        if !is_num(ch) {
                            self.errno = Option::Some(HttpErrno::InvalidVersion);
                            return index;
                        }

                        self.http_version.minor = ch - b'0';
                        self.state = State::ReqHttpMinor;
                    },
                    // minor HTTP version or end of request line
                    State::ReqHttpMinor => {
                        if ch == CR {
                            self.state = State::ReqLineAlmostDone;
                        } else if ch == LF {
                            self.state = State::HeaderFieldStart;
                        } else if !is_num(ch) {
                            // XXX allow spaces after digit?
                            self.errno = Option::Some(HttpErrno::InvalidVersion);
                            return index;
                        } else {
                            self.http_version.minor *= 10;
                            self.http_version.minor += ch - b'0';

                            if self.http_version.minor > 99 {
                                self.errno = Option::Some(HttpErrno::InvalidVersion);
                                return index;
                            }
                        }
                    },
                    // end of request line
                    State::ReqLineAlmostDone => {
                        if ch != LF {
                            self.errno = Option::Some(HttpErrno::LFExpected);
                            return index;
                        }

                        self.state = State::HeaderFieldStart;
                    },
                    State::HeaderFieldStart => {
                        if ch == CR {
                            self.state = State::HeadersAlmostDone;
                        } else if ch == LF {
                            // they might be just sending \n instead of \r\n,
                            // so this would be the second \n to denote
                            // the end of headers
                            self.state = State::HeadersAlmostDone;
                            retry = true;
                        } else {
                            let c : Option<u8> = token(self, ch);

                            if c.is_none() {
                                self.errno = Option::Some(HttpErrno::InvalidHeaderToken);
                                return index;
                            }

                            mark!(header_field_mark, index);
                            
                            self.index = 0;
                            self.state = State::HeaderField;

                            match c.unwrap() {
                                b'c' => self.header_state = HeaderState::C,
                                b'p' => self.header_state = HeaderState::MatchingProxyConnection,
                                b't' => self.header_state = HeaderState::MatchingTransferEncoding,
                                b'u' => self.header_state = HeaderState::MatchingUpgrade,
                                _    => self.header_state = HeaderState::General,
                            }
                        }
                    },
                    State::HeaderField => {
                        let c_opt : Option<u8> = token(self, ch);
                        if c_opt.is_some() {
                            let c : u8 = c_opt.unwrap();
                            match self.header_state {
                                HeaderState::General => (),
                                HeaderState::C => {
                                    self.index += 1;
                                    self.header_state = if c == b'o'{ 
                                        HeaderState::CO 
                                    } else {
                                        HeaderState::General
                                    };
                                },
                                HeaderState::CO => {
                                    self.index += 1;
                                    self.header_state = if c == b'n' {
                                        HeaderState::CON
                                    } else {
                                        HeaderState::General
                                    };
                                },
                                HeaderState::CON => {
                                    self.index += 1;
                                    match c {
                                        b'n' => self.header_state = HeaderState::MatchingConnection,
                                        b't' => self.header_state = HeaderState::MatchingContentLength,
                                        _    => self.header_state = HeaderState::General,
                                    }
                                },
                                // connection
                                HeaderState::MatchingConnection => {
                                    self.index += 1;
                                    if self.index >= CONNECTION.len() ||
                                        c != (CONNECTION.char_at(self.index) as u8) {
                                        self.header_state = HeaderState::General;
                                    } else if self.index == CONNECTION.len()-1 {
                                        self.header_state = HeaderState::Connection;
                                    }
                                },
                                // proxy-connection
                                HeaderState::MatchingProxyConnection => {
                                    self.index += 1;
                                    if self.index >= PROXY_CONNECTION.len() ||
                                        c != (PROXY_CONNECTION.char_at(self.index) as u8) {
                                        self.header_state = HeaderState::General;
                                    } else if self.index == PROXY_CONNECTION.len()-1 {
                                        self.header_state = HeaderState::Connection;
                                    }
                                },
                                // content-length
                                HeaderState::MatchingContentLength => {
                                    self.index += 1;
                                    if self.index >= CONTENT_LENGTH.len() ||
                                        c != (CONTENT_LENGTH.char_at(self.index) as u8) {
                                        self.header_state = HeaderState::General;
                                    } else if self.index == CONTENT_LENGTH.len()-1 {
                                        self.header_state = HeaderState::ContentLength;
                                    }
                                },
                                // transfer-encoding
                                HeaderState::MatchingTransferEncoding => {
                                    self.index += 1;
                                    if self.index >= TRANSFER_ENCODING.len() ||
                                        c != (TRANSFER_ENCODING.char_at(self.index) as u8) {
                                        self.header_state = HeaderState::General;
                                    } else if self.index == TRANSFER_ENCODING.len()-1 {
                                        self.header_state = HeaderState::TransferEncoding;
                                    }
                                },
                                // upgrade
                                HeaderState::MatchingUpgrade => {
                                    self.index += 1;
                                    if self.index >= UPGRADE.len() ||
                                        c != (UPGRADE.char_at(self.index) as u8) {
                                        self.header_state = HeaderState::General;
                                    } else if self.index == UPGRADE.len()-1 {
                                        self.header_state = HeaderState::Upgrade;
                                    }
                                },
                                HeaderState::Connection |
                                HeaderState::ContentLength |
                                HeaderState::TransferEncoding |
                                HeaderState::Upgrade => {
                                    if ch != b' ' {
                                        self.header_state = HeaderState::General;
                                    }
                                },
                                _ => {
                                    assert!(false, "Unknown header_state");
                                }
                            }
                        } else if ch == b':' {
                            self.state = State::HeaderValueDiscardWs;
                            assert_ok!(self);
                            callback_data!(self, header_field_mark,
                                cb.on_header_field(self, data.slice(header_field_mark.unwrap() as usize, index as usize)),
                                HttpErrno::CBHeaderField, index+1);
                        } else {
                            self.errno = Option::Some(HttpErrno::InvalidHeaderToken);
                            return index;
                        }
                    },
                    State::HeaderValueDiscardWs if ch == b' ' || ch == b'\t' ||
                        ch == CR || ch == LF => {
                        if ch == b' ' || ch == b'\t' {
                            ;
                        } else if ch == CR {
                            self.state = State::HeaderValueDiscardWsAlmostDone;
                        } else if ch == LF {
                            self.state = State::HeaderValueDiscardLws;
                        }
                    },
                    State::HeaderValueDiscardWs |
                    State::HeaderValueStart => {
                        mark!(header_value_mark, index);

                        self.state = State::HeaderValue;
                        self.index = 0;
                        
                        let c : u8 = lower(ch);

                        match self.header_state {
                            HeaderState::Upgrade => {
                                self.flags |= Flags::UPGRADE.as_u8();
                                self.header_state = HeaderState::General;
                            },
                            HeaderState::TransferEncoding => {
                                // looking for 'Transfer-Encoding: chunked
                                if c == b'c' {
                                    self.header_state = HeaderState::MatchingTransferEncodingChunked;
                                } else {
                                    self.header_state = HeaderState::General;
                                }
                            },
                            HeaderState::ContentLength => {
                                if !is_num(ch) {
                                    self.errno = Option::Some(HttpErrno::InvalidContentLength);
                                    return index;
                                }

                                self.content_length = (ch - b'0') as u64;
                            },
                            HeaderState::Connection => {
                                // looking for 'Connection: keep-alive
                                if c == b'k' {
                                    self.header_state = HeaderState::MatchingConnectionKeepAlive;
                                // looking for 'Connection: close
                                } else if c == b'c' {
                                    self.header_state = HeaderState::MatchingConnectionClose;
                                } else {
                                    self.header_state = HeaderState::General;
                                }
                            },
                            _ => self.header_state = HeaderState::General,
                        }
                    },
                    State::HeaderValue => {
                        if ch == CR {
                            self.state = State::HeaderAlmostDone;
                            assert_ok!(self);
                            callback_data!(self, header_value_mark,
                                cb.on_header_value(self, data.slice(header_value_mark.unwrap() as usize, index as usize)),
                                HttpErrno::CBHeaderValue, index+1);
                        } else if ch == LF {
                            self.state = State::HeaderAlmostDone;
                            assert_ok!(self);
                            callback_data!(self, header_value_mark,
                                cb.on_header_value(self, data.slice(header_value_mark.unwrap() as usize, index as usize)),
                                HttpErrno::CBHeaderValue, index);
                            retry = true;
                        } else {
                            let c : u8 = lower(ch);

                            match self.header_state {
                                HeaderState::General => (),
                                HeaderState::Connection | HeaderState::TransferEncoding => {
                                    assert!(false, "Shouldn't get here.");
                                },
                                HeaderState::ContentLength => {
                                    if ch != b' ' {
                                        if !is_num(ch) {
                                            self.errno = Option::Some(HttpErrno::InvalidContentLength);
                                            return index;
                                        }

                                        let mut t : u64 = self.content_length;
                                        t *= 10;
                                        t += (ch - b'0') as u64;

                                        // Overflow? Test against a conservative
                                        // limit for simplicity
                                        if (ULLONG_MAX - 10) / 10 < self.content_length {
                                            self.errno = Option::Some(HttpErrno::InvalidContentLength);
                                            return index;
                                        }

                                        self.content_length = t;
                                    }
                                },
                                // Transfer-Encoding: chunked
                                HeaderState::MatchingTransferEncodingChunked => {
                                    self.index += 1;
                                    if self.index >= CHUNKED.len() ||
                                        c != (CHUNKED.char_at(self.index) as u8) {
                                            self.header_state = HeaderState::General;
                                    } else if self.index == CHUNKED.len()-1 {
                                        self.header_state = HeaderState::TransferEncodingChunked;
                                    }
                                },
                                // looking for 'Connection: keep-alive
                                HeaderState::MatchingConnectionKeepAlive => {
                                    self.index += 1;
                                    if self.index >= KEEP_ALIVE.len() ||
                                        c != (KEEP_ALIVE.char_at(self.index) as u8) {
                                            self.header_state = HeaderState::General;
                                    } else if self.index == KEEP_ALIVE.len()-1 {
                                        self.header_state = HeaderState::ConnectionKeepAlive;
                                    }
                                }
                                // looking for 'Connection: close
                                HeaderState::MatchingConnectionClose => {
                                    self.index += 1;
                                    if self.index >= CLOSE.len() ||
                                        c != (CLOSE.char_at(self.index) as u8) {
                                            self.header_state = HeaderState::General;
                                    } else if self.index == CLOSE.len()-1 {
                                        self.header_state = HeaderState::ConnectionClose;
                                    }
                                },
                                HeaderState::TransferEncodingChunked |
                                HeaderState::ConnectionKeepAlive |
                                HeaderState::ConnectionClose => {
                                    if ch != b' ' {
                                        self.header_state = HeaderState::General;
                                    }
                                },
                                _ => {
                                    self.state = State::HeaderValue;
                                    self.header_state = HeaderState::General;
                                }
                            }
                        }
                    },
                    State::HeaderAlmostDone => {
                        strict_check!(self, ch != LF, index);

                        self.state = State::HeaderValueLws;
                    },
                    State::HeaderValueLws => {
                        if ch == b' ' || ch == b'\t' {
                            self.state = State::HeaderValueStart;
                            retry = true;
                        } else {
                            // finished the header
                            match self.header_state {
                                HeaderState::ConnectionKeepAlive => {
                                    self.flags |= Flags::CONNECTION_KEEP_ALIVE.as_u8();
                                },
                                HeaderState::ConnectionClose => {
                                    self.flags |= Flags::CONNECTION_CLOSE.as_u8();
                                },
                                HeaderState::TransferEncodingChunked => {
                                    self.flags |= Flags::CHUNKED.as_u8();
                                },
                                _ => (),
                            }

                            self.state = State::HeaderFieldStart;
                            retry = true;
                        }
                    },
                    State::HeaderValueDiscardWsAlmostDone => {
                        strict_check!(self, ch != LF, index);
                        self.state = State::HeaderValueDiscardLws;
                    },
                    State::HeaderValueDiscardLws => {
                        if ch == b' ' || ch == b'\t' {
                            self.state = State::HeaderValueDiscardWs;
                        } else {
                            // header value was empty
                            mark!(header_value_mark, index);
                            self.state = State::HeaderFieldStart;
                            assert_ok!(self);
                            callback_data!(self, header_value_mark,
                                cb.on_header_value(self, data.slice(header_value_mark.unwrap() as usize, index as usize)),
                                HttpErrno::CBHeaderValue, index);
                            retry = true;
                        }
                    },
                    State::HeadersAlmostDone => {
                        strict_check!(self, ch != LF, index);

                        if (self.flags & Flags::TRAILING.as_u8()) > 0 {
                            // End of a chunked request
                            self.state = new_message!(self);
                            assert_ok!(self);
                            callback!(self, cb.on_message_complete(self), 
                                      HttpErrno::CBMessageComplete);
                            if self.errno.is_some() {
                                return index+1;
                            }
                        } else {
                            self.state = State::HeadersDone;

                            // Set this here so that on_headers_complete()
                            // callbacks can see it
                            self.upgrade = (self.flags & Flags::UPGRADE.as_u8() != 0) ||
                                self.method == HttpMethod::Connect;

                            // Here we call the headers_complete callback. This is somewhat
                            // different than other callbacks because if the user returns 1, we
                            // will interpret that as saying that this message has no body. This
                            // is needed for the annoying case of recieving a response to a HEAD
                            // request.
                            // 
                            // We'd like to use CALLBACK_NOTIFY_NOADVANCE() here but we cannot,
                            // so
                            // we have to simulate it by handling a change in errno below.
                            //
                            // TODO can we handle this in our case?
                            match cb.on_headers_complete(self) {
                                Ok(CallbackDecision::Nothing) => (),
                                Ok(CallbackDecision::SkipBody) => self.flags |= Flags::SKIPBODY.as_u8(),
                                _     => {
                                    self.errno = Option::Some(HttpErrno::CBHeadersComplete);
                                    return index; // Error
                                },
                            }

                            if self.errno.is_some() {
                                return index;
                            }
                            retry = true;
                        }
                    },
                    State::HeadersDone => {
                        strict_check!(self, ch != LF, index);
                        self.nread = 0;

                        // Exit, The rest of the connect is in a different protocal
                        if self.upgrade {
                            self.state = new_message!(self);
                            assert_ok!(self);
                            callback!(self, cb.on_message_complete(self), 
                                      HttpErrno::CBMessageComplete);
                            if self.errno.is_some() {
                                return index+1;
                            }
                            return index+1;
                        }

                        if (self.flags & Flags::SKIPBODY.as_u8()) != 0 {
                            self.state = new_message!(self);
                            assert_ok!(self);
                            callback!(self, cb.on_message_complete(self), 
                                      HttpErrno::CBMessageComplete);
                            if self.errno.is_some() {
                                return index+1;
                            }
                        } else if (self.flags & Flags::CHUNKED.as_u8()) != 0 {
                            // chunked encoding - ignore Content-Length header
                            self.state = State::ChunkSizeStart;
                        } else {
                            if self.content_length == 0 {
                                // Content-Length header given but zero: Content-Length: 0\r\n
                                self.state = new_message!(self);
                                assert_ok!(self);
                                callback!(self, cb.on_message_complete(self), 
                                          HttpErrno::CBMessageComplete);
                                if self.errno.is_some() {
                                    return index+1;
                                }
                            } else if self.content_length != ULLONG_MAX {
                                // Content-Length header given and non-zero
                                self.state = State::BodyIdentity;
                            } else {
                                if self.tp == HttpParserType::Request ||
                                    !self.http_message_needs_eof() {
                                    // Assume content-length 0 - read the next
                                    self.state = new_message!(self);
                                    assert_ok!(self);
                                    callback!(self, cb.on_message_complete(self), 
                                              HttpErrno::CBMessageComplete);
                                    if self.errno.is_some() {
                                        return index+1;
                                    }
                                } else {
                                    // Read body until EOF
                                    self.state = State::BodyIdentityEof;
                                }
                            }
                        }
                    },
                    State::BodyIdentity => {
                        let to_read : u64 = cmp::min(self.content_length,
                                                    (len - index) as u64);
                        assert!(self.content_length != 0 &&
                                self.content_length != ULLONG_MAX);

                        // The difference between advancing content_length and p is because
                        // the latter will automaticaly advance on the next loop iteration.
                        // Further, if content_length ends up at 0, we want to see the last
                        // byte again for our message complete callback.
                        mark!(body_mark, index);
                        self.content_length -= to_read;

                        index += to_read - 1;

                        if self.content_length == 0 {
                            self.state = State::MessageDone;

                            // Mimic CALLBACK_DATA_NOADVANCE() but with one extra byte.
                            //
                            // The alternative to doing this is to wait for the next byte to
                            // trigger the data callback, just as in every other case. The
                            // problem with this is that this makes it difficult for the test
                            // harness to distinguish between complete-on-EOF and
                            // complete-on-length. It's not clear that this distinction is
                            // important for applications, but let's keep it for now.
                            assert_ok!(self);
                            callback_data!(self, body_mark,
                                cb.on_body(self, data.slice(body_mark.unwrap() as usize, (index + 1) as usize)),
                                HttpErrno::CBBody, index);
                            retry = true;
                        }
                    },
                    // read until EOF
                    State::BodyIdentityEof => {
                        mark!(body_mark, index);
                        index = len - 1;
                    },
                    State::MessageDone => {
                        self.state = new_message!(self);
                        assert_ok!(self);
                        callback!(self, cb.on_message_complete(self), 
                                  HttpErrno::CBMessageComplete);
                        if self.errno.is_some() {
                            return index+1;
                        }
                    },
                    State::ChunkSizeStart => {
                        assert!(self.nread == 1);
                        assert!(self.flags & Flags::CHUNKED.as_u8() != 0);

                        let unhex_val : i8 = UNHEX[ch as usize];
                        if unhex_val == -1 {
                            self.errno = Option::Some(HttpErrno::InvalidChunkSize);
                            return index;
                        }

                        self.content_length = unhex_val as u64;
                        self.state = State::ChunkSize;
                    },
                    State::ChunkSize => {
                        assert!(self.flags & Flags::CHUNKED.as_u8() != 0);

                        if ch == CR {
                            self.state = State::ChunkSizeAlmostDone;
                        } else {
                            let unhex_val : i8 = UNHEX[ch as usize];
                            if unhex_val == -1 {
                                if ch == b';' || ch == b' ' {
                                    self.state = State::ChunkParameters;
                                } else {
                                    self.errno = Option::Some(HttpErrno::InvalidChunkSize);
                                    return index;
                                }
                            } else {
                                let mut t : u64 = self.content_length;
                                t *= 16;
                                t += unhex_val as u64;

                                // Overflow? Test against a conservative limit for simplicity
                                if (ULLONG_MAX - 16)/16 < self.content_length {
                                    self.errno = Option::Some(HttpErrno::InvalidContentLength);
                                    return index;
                                }

                                self.content_length = t;
                            }
                        }
                    },
                    State::ChunkParameters => {
                        assert!(self.flags & Flags::CHUNKED.as_u8() != 0);
                        // just ignore this shit. TODO check for overflow
                        if ch == CR {
                            self.state = State::ChunkSizeAlmostDone;
                        }
                    },
                    State::ChunkSizeAlmostDone => {
                        assert!(self.flags & Flags::CHUNKED.as_u8() != 0);
                        strict_check!(self, ch != LF, index);

                        self.nread = 0;

                        if self.content_length == 0 {
                            self.flags |= Flags::TRAILING.as_u8();
                            self.state = State::HeaderFieldStart;
                        } else {
                            self.state = State::ChunkData;
                        }
                    },
                    State::ChunkData => {
                        let to_read : u64 = cmp::min(self.content_length,
                                                         len - index);
                        assert!(self.flags & Flags::CHUNKED.as_u8() != 0);
                        assert!(self.content_length != 0 &&
                                self.content_length != ULLONG_MAX);

                        // See the explanation in s_body_identity for why the content
                        // length and data pointers are managed this way.
                        mark!(body_mark, index);
                        self.content_length -= to_read;
                        index += to_read - 1;

                        if self.content_length == 0 {
                            self.state = State::ChunkDataAlmostDone;
                        }
                    },
                    State::ChunkDataAlmostDone => {
                        assert!(self.flags & Flags::CHUNKED.as_u8() != 0);
                        assert!(self.content_length == 0);
                        strict_check!(self, ch != CR, index);
                        self.state = State::ChunkDataDone;

                        assert_ok!(self);
                        callback_data!(self, body_mark,
                            cb.on_body(self, data.slice(body_mark.unwrap() as usize, index as usize)),
                            HttpErrno::CBBody, index+1);
                    },
                    State::ChunkDataDone => {
                        assert!(self.flags & Flags::CHUNKED.as_u8() != 0);
                        strict_check!(self, ch != LF, index);
                        self.nread = 0;
                        self.state = State::ChunkSizeStart;
                    },
                    //_ => {
                    //    assert!(false, "unhandled state");
                    //    self.errno = HttpErrno::InvalidInternalState;
                    //    return index;
                    //},
                }

                if !retry {
                    break;
                }
            }
            index += 1;
        }

        // Run callbacks for any marks that we have leftover after we ran our of
        // bytes. There should be at most one of these set, so it's OK to invoke
        // them in series (unset marks will not result in callbacks).
        //
        // We use the NOADVANCE() variety of callbacks here because 'p' has already
        // overflowed 'data' and this allows us to correct for the off-by-one that
        // we'd otherwise have (since CALLBACK_DATA() is meant to be run with a 'p'
        // value that's in-bounds).
        assert!((if header_field_mark.is_some() { 1u8 } else { 0 }) +
                (if header_value_mark.is_some() { 1 } else { 0 }) +
                (if url_mark.is_some() { 1 } else { 0 }) +
                (if body_mark.is_some() { 1 } else { 0 }) +
                (if status_mark.is_some() { 1 } else { 0 }) <= 1);

        callback_data!(self, header_field_mark,
            cb.on_header_field(self, data.slice(header_field_mark.unwrap() as usize, index as usize)),
            HttpErrno::CBHeaderField, index);
        callback_data!(self, header_value_mark,
            cb.on_header_value(self, data.slice(header_value_mark.unwrap() as usize, index as usize)),
            HttpErrno::CBHeaderValue, index);
        callback_data!(self, url_mark,
            cb.on_url(self, data.slice(url_mark.unwrap() as usize, index as usize)),
            HttpErrno::CBUrl, index);
        callback_data!(self, body_mark,
            cb.on_body(self, data.slice(body_mark.unwrap() as usize, index as usize)),
            HttpErrno::CBBody, index);
        callback_data!(self, status_mark,
            cb.on_status(self, data.slice(status_mark.unwrap() as usize, index as usize)),
            HttpErrno::CBStatus, index);
        len
    }

    pub fn http_body_is_final(&self) -> bool {
        self.state == State::MessageDone
    }

    pub fn pause(&mut self, pause : bool) {
        if self.errno.is_none() || self.errno == Option::Some(HttpErrno::Paused) {
            self.errno = if pause {
                Option::Some(HttpErrno::Paused)
            } else {
                Option::None
            };
        } else {
            panic!("Attempting to pause parser in error state");
        }
    }

    // Our URL parser
    fn parse_url_char(&self, s : State, ch : u8) -> State {
        if ch == b' ' || ch == b'\r' || ch == b'\n' {
            return State::Dead;
        }

        if self.strict {
            if ch == b'\t' || ch == b'\x0C' {   // '\x0C' = '\f' 
                return State::Dead;
            }
        }

        match s {
            State::ReqSpacesBeforeUrl => {
                // Proxied requests are followed by scheme of an absolute URI (alpha).
                // All methods except CONNECT are followed by '/' or '*'.

                if ch == b'/' || ch == b'*' {
                    return State::ReqPath;
                }

                if is_alpha(ch) {
                    return State::ReqSchema;
                }
            },
            State::ReqSchema => {
                if is_alpha(ch) {
                    return s;
                }

                if ch == b':' {
                    return State::ReqSchemaSlash;
                }
            },
            State::ReqSchemaSlash => {
                if ch == b'/' {
                    return State::ReqSchemaSlashSlash;
                }
            },
            State::ReqSchemaSlashSlash => {
                if ch == b'/' {
                    return State::ReqServerStart;
                }
            },
            State::ReqServerWithAt if ch == b'@' => return State::Dead,
            State::ReqServerWithAt | State::ReqServerStart | State::ReqServer => {
                if ch == b'/' {
                    return State::ReqPath;
                }

                if ch == b'?' {
                    return State::ReqQueryStringStart;
                }

                if ch == b'@' {
                    return State::ReqServerWithAt;
                }

                if is_userinfo_char(ch) || ch == b'[' || ch == b']' {
                    return State::ReqServer;
                }
            },
            State::ReqPath => {
                if is_url_char(self, ch) {
                    return s;
                }

                match ch {
                    b'?' => return State::ReqQueryStringStart,
                    b'#' => return State::ReqFragmentStart,
                    _    => (),
                }
            },
            State::ReqQueryStringStart | State::ReqQueryString => {
                if is_url_char(self, ch) {
                    return State::ReqQueryString;
                }

                match ch {
                    b'?' => return State::ReqQueryString, // allow extra '?' in query string
                    b'#' => return State::ReqFragmentStart,
                    _    => (),
                }
            },
            State::ReqFragmentStart => {
                if is_url_char(self, ch) {
                    return State::ReqFragment;
                }

                match ch {
                    b'?' => return State::ReqFragment,
                    b'#' => return s,
                    _    => (),
                }
            },
            State::ReqFragment => {
                if is_url_char(self, ch) {
                    return s;
                }

                if ch == b'?' || ch == b'#' {
                    return s;
                }
            },
            _ => (),
        }

        // We should never fall out of the switch above unless there's an error
        return State::Dead;
    }

    // Does the parser need to see an EOF to find the end of the message?
    fn http_message_needs_eof(&self) -> bool {
        if self.tp == HttpParserType::Request {
            return false
        }

        // See RFC 2616 section 4.4
        if self.status_code / 100 == 1 || // 1xx e.g. Continue
            self.status_code == 204 ||    // No Content
            self.status_code == 304 ||    // Not Modified
            (self.flags & Flags::SKIPBODY.as_u8()) != 0 {// response to a HEAD request
            return false
        }

        if (self.flags & Flags::CHUNKED.as_u8() != 0) ||
            self.content_length != ULLONG_MAX {
            return false
        }

        true
    }

    pub fn http_should_keep_alive(&self) -> bool {
        if self.http_version.major > 0 && self.http_version.minor > 0 {
            // HTTP/1.1
            if (self.flags & Flags::CONNECTION_CLOSE.as_u8()) != 0 {
                return false
            }
        } else {
            // HTTP/1.0 or earlier
            if (self.flags & Flags::CONNECTION_KEEP_ALIVE.as_u8()) == 0 {
                return false
            }
        }

        !self.http_message_needs_eof()
    }

}