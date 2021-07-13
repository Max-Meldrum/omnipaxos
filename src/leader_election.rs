use std::fmt::Debug;

/// Rounds in Omni-Paxos must be totally ordered.
pub trait Round: Clone + Debug + Ord + Default + Send + 'static {}

/// Leader event that indicates a leader has been elected. Should be created when the user-defined BLE algorithm
/// outputs a leader event. Should be then handled in Omni-Paxos by calling [`crate::paxos::Paxos::handle_leader()`].
#[derive(Copy, Clone, Debug)]
pub struct Leader<R>
where
    R: Round,
{
    /// The pid of the elected leader.
    pub pid: u64,
    /// The round in which `pid` is elected in.
    pub round: R,
}

impl<R> Leader<R>
where
    R: Round,
{
    /// Constructor for [`Leader`].
    pub fn with(pid: u64, round: R) -> Self {
        Leader { pid, round }
    }
}

/// Ballot Leader Election algorithm for electing new leaders
pub mod ballot_leader_election {
    use crate::leader_election::{Leader, Round};
    use messages::{BLEMessage, HeartbeatMsg, HeartbeatReply, HeartbeatRequest};

    /// Used to define an epoch
    #[derive(Clone, Copy, Eq, Debug, Default, Ord, PartialOrd, PartialEq)]
    pub struct Ballot {
        /// Ballot number
        pub n: u32,
        /// The pid of the process
        pub pid: u64,
    }

    impl Ballot {
        /// Creates a new Ballot
        /// # Arguments
        /// * `n` - Ballot number.
        /// * `pid` -  Used as tiebreaker for total ordering of ballots.
        pub fn with(n: u32, pid: u64) -> Ballot {
            Ballot { n, pid }
        }
    }

    impl Round for Ballot {}

    /// A Ballot Leader Election component. Used in conjunction with Omni-Paxos handles the election of a leader for a group of omni-paxos replicas,
    /// incoming messages and produces outgoing messages that the user has to fetch periodically and send using a network implementation.
    /// User also has to periodically fetch the decided entries that are guaranteed to be strongly consistent and linearizable, and therefore also safe to be used in the higher level application.
    pub struct BallotLeaderElection {
        pid: u64,
        peers: Vec<u64>,
        hb_round: u32,
        ballots: Vec<(Ballot, bool)>,
        current_ballot: Ballot, // (round, pid)
        majority_connected: bool,
        leader: Option<Ballot>,
        hb_current_delay: u64,
        hb_delay: u64,
        increment_delay: u64,
        /// The majority of replicas inside a cluster
        majority: usize,
        quick_timeout: bool,
        /// A factor used in the beginning for a shorter hb_delay.
        /// Used to faster elect a leader when starting up.
        /// If used, then hb_delay is set to hb_delay/initial_delay_factor until the first leader is elected.
        initial_delay_factor: u64,
        ticks_elapsed: u64,
        outgoing: Vec<BLEMessage>,
    }

    impl BallotLeaderElection {
        /// Construct a new BallotLeaderComponent
        pub fn with(
            peers: Vec<u64>,
            pid: u64,
            hb_delay: u64,
            increment_delay: u64,
            quick_timeout: bool,
            initial_leader: Option<Leader<Ballot>>,
            initial_delay_factor: Option<u64>,
        ) -> BallotLeaderElection {
            let n = &peers.len() + 1;
            let (leader, initial_ballot) = match initial_leader {
                Some(l) => {
                    let leader_ballot = Ballot::with(l.round.n, l.pid);
                    let initial_ballot = if l.pid == pid {
                        leader_ballot
                    } else {
                        Ballot::with(0, pid)
                    };
                    (Some(leader_ballot), initial_ballot)
                }
                None => {
                    let initial_ballot = Ballot::with(0, pid);
                    (None, initial_ballot)
                }
            };
            BallotLeaderElection {
                pid,
                majority: n / 2 + 1, // +1 because peers is exclusive ourselves
                peers,
                hb_round: 0,
                ballots: Vec::with_capacity(n),
                current_ballot: initial_ballot,
                majority_connected: true,
                leader,
                hb_current_delay: hb_delay,
                hb_delay,
                increment_delay,
                quick_timeout,
                initial_delay_factor: initial_delay_factor.unwrap_or(1),
                ticks_elapsed: 0,
                outgoing: vec![],
            }
        }

        /// Get the current elected leader
        pub fn get_leader(&self) -> Option<Leader<Ballot>> {
            self.leader
                .and_then(|ballot: Ballot| -> Option<Leader<Ballot>> {
                    Some(Leader::with(ballot.pid, ballot))
                })
        }

        /// tick is run by all servers to simulate the passage of time
        /// Returns an Option with the elected leader otherwise None
        pub fn tick(&mut self) -> Option<Leader<Ballot>> {
            self.ticks_elapsed += 1;

            if self.ticks_elapsed >= self.hb_current_delay {
                self.ticks_elapsed = 0;
                self.hb_timeout()
            } else {
                None
            }
        }

        /// Handle an incoming message.
        /// # Arguments
        /// * `m` - .
        pub fn handle(&mut self, m: BLEMessage) {
            match m.msg {
                HeartbeatMsg::Request(req) => self.handle_request(m.from, req),
                HeartbeatMsg::Reply(rep) => self.handle_reply(rep),
            }
        }

        /// Sets initial state after creation. Should only be used before being started.
        /// # Arguments
        /// * `l` - Initial leader.
        pub fn set_initial_leader(&mut self, l: Leader<Ballot>) {
            assert!(self.leader.is_none());
            let leader_ballot = Ballot::with(l.round.n, l.pid);
            self.leader = Some(leader_ballot);
            if l.pid == self.pid {
                self.current_ballot = leader_ballot;
                self.majority_connected = true;
            } else {
                self.current_ballot = Ballot::with(0, self.pid);
                self.majority_connected = false;
            };
            self.quick_timeout = false;
        }

        fn check_leader(&mut self) -> Option<Leader<Ballot>> {
            let ballots = std::mem::take(&mut self.ballots);
            let top_ballot = ballots
                .into_iter()
                .filter_map(
                    |(ballot, candidate)| {
                        if candidate {
                            Some(ballot)
                        } else {
                            None
                        }
                    },
                )
                .max()
                .unwrap_or_default();

            if top_ballot < self.leader.unwrap_or_default() {
                // did not get HB from leader
                self.current_ballot.n = self.leader.unwrap_or_default().n + 1;
                self.leader = None;
                self.majority_connected = true;

                None
            } else if self.leader != Some(top_ballot) {
                // got a new leader with greater ballot
                self.quick_timeout = false;
                self.leader = Some(top_ballot);
                let top_pid = top_ballot.pid;
                if self.pid == top_pid {
                    self.majority_connected = true;
                } else {
                    self.majority_connected = false;
                }

                Some(Leader::with(top_pid, top_ballot))
            } else {
                None
            }
        }

        fn new_hb_round(&mut self) {
            self.hb_current_delay = if self.quick_timeout {
                // use short timeout if still no first leader
                self.hb_delay / self.initial_delay_factor
            } else {
                self.hb_delay
            };

            self.hb_round += 1;
            for peer in &self.peers {
                let hb_request = HeartbeatRequest::with(self.hb_round);

                self.outgoing.push(BLEMessage::with(
                    *peer,
                    self.pid,
                    HeartbeatMsg::Request(hb_request),
                ));
            }
        }

        fn hb_timeout(&mut self) -> Option<Leader<Ballot>> {
            let result: Option<Leader<Ballot>> = if self.ballots.len() + 1 >= self.majority {
                self.ballots
                    .push((self.current_ballot, self.majority_connected));
                self.check_leader()
            } else {
                self.ballots.clear();
                self.majority_connected = false;
                None
            };
            self.new_hb_round();

            result
        }

        fn handle_request(&mut self, from: u64, req: HeartbeatRequest) {
            let hb_reply =
                HeartbeatReply::with(req.round, self.current_ballot, self.majority_connected);

            self.outgoing.push(BLEMessage::with(
                self.pid,
                from,
                HeartbeatMsg::Reply(hb_reply),
            ));
        }

        fn handle_reply(&mut self, rep: HeartbeatReply) {
            if rep.round == self.hb_round {
                self.ballots.push((rep.ballot, rep.majority_connected));
            } else {
                self.hb_delay += self.increment_delay;
            }
        }
    }

    /// The different messages BLE uses to communicate with other replicas.
    pub mod messages {
        use crate::leader_election::ballot_leader_election::Ballot;

        /// An enum for all the different BLE message types.
        #[allow(missing_docs)]
        #[derive(Clone, Debug)]
        pub enum HeartbeatMsg {
            Request(HeartbeatRequest),
            Reply(HeartbeatReply),
        }

        /// Requests a reply from all the other replicas.
        #[derive(Clone, Debug)]
        pub struct HeartbeatRequest {
            /// Number of the current round.
            pub round: u32,
        }

        impl HeartbeatRequest {
            /// Creates a new HeartbeatRequest
            /// # Arguments
            /// * `round` - number of the current round.
            pub fn with(round: u32) -> HeartbeatRequest {
                HeartbeatRequest { round }
            }
        }

        /// Replies
        #[derive(Clone, Debug)]
        pub struct HeartbeatReply {
            /// Number of the current round.
            pub round: u32,
            /// Ballot of a replica.
            pub ballot: Ballot,
            /// States if the replica is a candidate to become a leader.
            pub majority_connected: bool,
        }

        impl HeartbeatReply {
            /// Creates a new HeartbeatRequest
            /// # Arguments
            /// * `round` - Number of the current round.
            /// * `ballot` -  Ballot of a replica.
            /// * `majority_connected` -  States if the replica is majority_connected to become a leader.
            pub fn with(round: u32, ballot: Ballot, majority_connected: bool) -> HeartbeatReply {
                HeartbeatReply {
                    round,
                    ballot,
                    majority_connected,
                }
            }
        }

        /// A struct for a Paxos message that also includes sender and receiver.
        #[derive(Clone, Debug)]
        pub struct BLEMessage {
            /// Sender of `msg`.
            pub from: u64,
            /// Receiver of `msg`.
            pub to: u64,
            /// The message content.
            pub msg: HeartbeatMsg,
        }

        impl BLEMessage {
            /// Creates a BLE message.
            /// # Arguments
            /// * `from` - Sender of `msg`.
            /// * `to` -  Receiver of `msg`.
            /// * `msg` -  The message content.
            pub fn with(from: u64, to: u64, msg: HeartbeatMsg) -> Self {
                BLEMessage { from, to, msg }
            }
        }
    }
}
