use core::{convert::TryFrom, fmt};
use std::collections::HashMap;

use rand_core::{RngCore, CryptoRng};

use ff::{Field, PrimeField};
use group::Group;

use crate::{Curve, MultisigParams, MultisigKeys, FrostError, validate_map};

#[allow(non_snake_case)]
fn challenge<C: Curve>(l: u16, context: &str, R: &[u8], Am: &[u8]) -> C::F {
  let mut c = Vec::with_capacity(2 + context.len() + R.len() + Am.len());
  c.extend(l.to_be_bytes());
  c.extend(context.as_bytes());
  c.extend(R);  // R
  c.extend(Am); // A of the first commitment, which is what we're proving we have the private key
                // for
                // m of the rest of the commitments, authenticating them
  C::hash_to_F(&c)
}

// Implements steps 1 through 3 of round 1 of FROST DKG. Returns the coefficients, commitments, and
// the serialized commitments to be broadcasted over an authenticated channel to all parties
fn generate_key_r1<R: RngCore + CryptoRng, C: Curve>(
  rng: &mut R,
  params: &MultisigParams,
  context: &str,
) -> (Vec<C::F>, Vec<u8>) {
  let t = usize::from(params.t);
  let mut coefficients = Vec::with_capacity(t);
  let mut commitments = Vec::with_capacity(t);
  let mut serialized = Vec::with_capacity((C::G_len() * t) + C::G_len() + C::F_len());

  for i in 0 .. t {
    // Step 1: Generate t random values to form a polynomial with
    coefficients.push(C::F::random(&mut *rng));
    // Step 3: Generate public commitments
    commitments.push(C::generator_table() * coefficients[i]);
    // Serialize them for publication
    serialized.extend(&C::G_to_bytes(&commitments[i]));
  }

  // Step 2: Provide a proof of knowledge
  // This can be deterministic as the PoK is a singleton never opened up to cooperative discussion
  // There's also no reason to spend the time and effort to make this deterministic besides a
  // general obsession with canonicity and determinism
  let r = C::F::random(rng);
  #[allow(non_snake_case)]
  let R = C::generator_table() * r;
  let s = r + (
    coefficients[0] * challenge::<C>(params.i(), context, &C::G_to_bytes(&R), &serialized)
  );

  serialized.extend(&C::G_to_bytes(&R));
  serialized.extend(&C::F_to_bytes(&s));

  // Step 4: Broadcast
  (coefficients, serialized)
}

// Verify the received data from the first round of key generation
fn verify_r1<R: RngCore + CryptoRng, C: Curve>(
  rng: &mut R,
  params: &MultisigParams,
  context: &str,
  our_commitments: Vec<u8>,
  mut serialized: HashMap<u16, Vec<u8>>,
) -> Result<HashMap<u16, Vec<C::G>>, FrostError> {
  validate_map(
    &mut serialized,
    &(1 ..= params.n()).into_iter().collect::<Vec<_>>(),
    (params.i(), our_commitments)
  )?;

  let commitments_len = usize::from(params.t()) * C::G_len();

  let mut commitments = HashMap::new();

  #[allow(non_snake_case)]
  let R_bytes = |l| &serialized[&l][commitments_len .. commitments_len + C::G_len()];
  #[allow(non_snake_case)]
  let R = |l| C::G_from_slice(R_bytes(l)).map_err(|_| FrostError::InvalidProofOfKnowledge(l));
  #[allow(non_snake_case)]
  let Am = |l| &serialized[&l][0 .. commitments_len];

  let s = |l| C::F_from_slice(
    &serialized[&l][commitments_len + C::G_len() ..]
  ).map_err(|_| FrostError::InvalidProofOfKnowledge(l));

  let mut first = true;
  let mut scalars = Vec::with_capacity((usize::from(params.n()) - 1) * 3);
  let mut points = Vec::with_capacity((usize::from(params.n()) - 1) * 3);
  for l in 1 ..= params.n() {
    let mut these_commitments = vec![];
    for c in 0 .. usize::from(params.t()) {
      these_commitments.push(
        C::G_from_slice(
          &serialized[&l][(c * C::G_len()) .. ((c + 1) * C::G_len())]
        ).map_err(|_| FrostError::InvalidCommitment(l.try_into().unwrap()))?
      );
    }
    commitments.insert(l, these_commitments);

    // Don't bother validating our own proof of knowledge
    if l == params.i() {
      continue;
    }

    // Step 5: Validate each proof of knowledge (prep)
    let mut u = C::F::one();
    if !first {
      u = C::F::random(&mut *rng);
    }

    // uR
    scalars.push(u);
    points.push(R(l)?);

    // -usG
    scalars.push(-s(l)? * u);
    points.push(C::generator());

    // ucA
    let c = challenge::<C>(l, context, R_bytes(l), Am(l));
    scalars.push(if first { first = false; c } else { c * u});
    points.push(commitments[&l][0]);
  }

  // Step 5: Implementation
  // Uses batch verification to optimize the success case dramatically
  // On failure, the cost is now this + blame, yet that should happen infrequently
  // s = r + ca
  // sG == R + cA
  // R + cA - sG == 0
  if C::multiexp_vartime(&scalars, &points) != C::G::identity() {
    for l in 1 ..= params.n() {
      if l == params.i() {
        continue;
      }

      if (C::generator_table() * s(l)?) != (
        R(l)? + (commitments[&l][0] * challenge::<C>(l, context, R_bytes(l), Am(l)))
      ) {
        Err(FrostError::InvalidProofOfKnowledge(l))?;
      }
    }

    Err(FrostError::InternalError("batch validation is broken".to_string()))?;
  }

  Ok(commitments)
}

fn polynomial<F: PrimeField>(
  coefficients: &[F],
  l: u16
) -> F {
  let l = F::from(u64::from(l));
  let mut share = F::zero();
  for (idx, coefficient) in coefficients.iter().rev().enumerate() {
    share += coefficient;
    if idx != (coefficients.len() - 1) {
      share *= l;
    }
  }
  share
}

// Implements round 1, step 5 and round 2, step 1 of FROST key generation
// Returns our secret share part, commitments for the next step, and a vector for each
// counterparty to receive
fn generate_key_r2<R: RngCore + CryptoRng, C: Curve>(
  rng: &mut R,
  params: &MultisigParams,
  context: &str,
  coefficients: Vec<C::F>,
  our_commitments: Vec<u8>,
  commitments: HashMap<u16, Vec<u8>>,
) -> Result<(C::F, HashMap<u16, Vec<C::G>>, HashMap<u16, Vec<u8>>), FrostError> {
  let commitments = verify_r1::<R, C>(rng, params, context, our_commitments, commitments)?;

  // Step 1: Generate secret shares for all other parties
  let mut res = HashMap::new();
  for l in 1 ..= params.n() {
    // Don't insert our own shares to the byte buffer which is meant to be sent around
    // An app developer could accidentally send it. Best to keep this black boxed
    if l == params.i() {
      continue;
    }

    res.insert(l, C::F_to_bytes(&polynomial(&coefficients, l)));
  }

  // Calculate our own share
  let share = polynomial(&coefficients, params.i());

  // The secret shares are discarded here, not cleared. While any system which leaves its memory
  // accessible is likely totally lost already, making the distinction meaningless when the key gen
  // system acts as the signer system and therefore actively holds the signing key anyways, it
  // should be overwritten with /dev/urandom in the name of security (which still doesn't meet
  // requirements for secure data deletion yet those requirements expect hardware access which is
  // far past what this library can reasonably counter)
  // TODO: Zero out the coefficients

  Ok((share, commitments, res))
}

/// Finishes round 2 and returns both the secret share and the serialized public key.
/// This key is not usable until all parties confirm they have completed the protocol without
/// issue, yet simply confirming protocol completion without issue is enough to confirm the same
/// key was generated as long as a lack of duplicated commitments was also confirmed when they were
/// broadcasted initially
fn complete_r2<C: Curve>(
  params: MultisigParams,
  share: C::F,
  commitments: HashMap<u16, Vec<C::G>>,
  // Vec to preserve ownership
  mut serialized: HashMap<u16, Vec<u8>>,
) -> Result<MultisigKeys<C>, FrostError> {
  validate_map(
    &mut serialized,
    &(1 ..= params.n()).into_iter().collect::<Vec<_>>(),
    (params.i(), C::F_to_bytes(&share))
  )?;

  // Step 2. Verify each share
  let mut shares = HashMap::new();
  for (l, share) in serialized {
    shares.insert(l, C::F_from_slice(&share).map_err(|_| FrostError::InvalidShare(params.i()))?);
  }

  for (l, share) in &shares {
    if *l == params.i() {
      continue;
    }

    let i_scalar = C::F::from(params.i.into());
    let mut exp = C::F::one();
    let mut exps = Vec::with_capacity(usize::from(params.t()));
    for _ in 0 .. params.t() {
      exps.push(exp);
      exp *= i_scalar;
    }

    // Doesn't use multiexp_vartime with -shares[l] due to not being able to push to commitments
    if C::multiexp_vartime(&exps, &commitments[&l]) != (C::generator_table() * *share) {
      Err(FrostError::InvalidCommitment(*l))?;
    }
  }

  // TODO: Clear the original share

  let mut secret_share = C::F::zero();
  for (_, share) in shares {
    secret_share += share;
  }

  let mut verification_shares = HashMap::new();
  for l in 1 ..= params.n() {
    let mut exps = vec![];
    let mut cs = vec![];
    for i in 1 ..= params.n() {
      for j in 0 .. params.t() {
        let mut exp = C::F::one();
        for _ in 0 .. j {
          exp *= C::F::from(u64::try_from(l).unwrap());
        }
        exps.push(exp);
        cs.push(commitments[&i][usize::from(j)]);
      }
    }
    verification_shares.insert(l, C::multiexp_vartime(&exps, &cs));
  }
  debug_assert_eq!(C::generator_table() * secret_share, verification_shares[&params.i()]);

  let group_key = commitments.iter().map(|(_, commitments)| commitments[0]).sum();

  // TODO: Clear serialized and shares

  Ok(MultisigKeys { params, secret_share, group_key, verification_shares, offset: None } )
}

/// State of a Key Generation machine
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
  Fresh,
  GeneratedCoefficients,
  GeneratedSecretShares,
  Complete,
}

impl fmt::Display for State {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{:?}", self)
  }
}

/// State machine which manages key generation
#[allow(non_snake_case)]
pub struct StateMachine<C: Curve> {
  params: MultisigParams,
  context: String,
  state: State,
  coefficients: Option<Vec<C::F>>,
  our_commitments: Option<Vec<u8>>,
  secret: Option<C::F>,
  commitments: Option<HashMap<u16, Vec<C::G>>>
}

impl<C: Curve> StateMachine<C> {
  /// Creates a new machine to generate a key for the specified curve in the specified multisig
  // The context string must be unique among multisigs
  pub fn new(params: MultisigParams, context: String) -> StateMachine<C> {
    StateMachine {
      params,
      context,
      state: State::Fresh,
      coefficients: None,
      our_commitments: None,
      secret: None,
      commitments: None
    }
  }

  /// Start generating a key according to the FROST DKG spec
  /// Returns a serialized list of commitments to be sent to all parties over an authenticated
  /// channel. If any party submits multiple sets of commitments, they MUST be treated as malicious
  pub fn generate_coefficients<R: RngCore + CryptoRng>(
    &mut self,
    rng: &mut R
  ) -> Result<Vec<u8>, FrostError> {
    if self.state != State::Fresh {
      Err(FrostError::InvalidKeyGenTransition(State::Fresh, self.state))?;
    }

    let (coefficients, serialized) = generate_key_r1::<R, C>(
      rng,
      &self.params,
      &self.context,
    );

    self.coefficients = Some(coefficients);
    self.our_commitments = Some(serialized.clone());
    self.state = State::GeneratedCoefficients;
    Ok(serialized)
  }

  /// Continue generating a key
  /// Takes in everyone else's commitments, which are expected to be in a Vec where participant
  /// index = Vec index. An empty vector is expected at index 0 to allow for this. An empty vector
  /// is also expected at index i which is locally handled. Returns a byte vector representing a
  /// secret share for each other participant which should be encrypted before sending
  pub fn generate_secret_shares<R: RngCore + CryptoRng>(
    &mut self,
    rng: &mut R,
    commitments: HashMap<u16, Vec<u8>>,
  ) -> Result<HashMap<u16, Vec<u8>>, FrostError> {
    if self.state != State::GeneratedCoefficients {
      Err(FrostError::InvalidKeyGenTransition(State::GeneratedCoefficients, self.state))?;
    }

    let (secret, commitments, shares) = generate_key_r2::<R, C>(
      rng,
      &self.params,
      &self.context,
      self.coefficients.take().unwrap(),
      self.our_commitments.take().unwrap(),
      commitments,
    )?;

    self.secret = Some(secret);
    self.commitments = Some(commitments);
    self.state = State::GeneratedSecretShares;
    Ok(shares)
  }

  /// Complete key generation
  /// Takes in everyone elses' shares submitted to us as a Vec, expecting participant index =
  /// Vec index with an empty vector at index 0 and index i. Returns a byte vector representing the
  /// group's public key, while setting a valid secret share inside the machine. > t participants
  /// must report completion without issue before this key can be considered usable, yet you should
  /// wait for all participants to report as such
  pub fn complete(
    &mut self,
    shares: HashMap<u16, Vec<u8>>,
  ) -> Result<MultisigKeys<C>, FrostError> {
    if self.state != State::GeneratedSecretShares {
      Err(FrostError::InvalidKeyGenTransition(State::GeneratedSecretShares, self.state))?;
    }

    let keys = complete_r2(
      self.params,
      self.secret.take().unwrap(),
      self.commitments.take().unwrap(),
      shares,
    )?;

    self.state = State::Complete;
    Ok(keys)
  }

  pub fn params(&self) -> MultisigParams {
    self.params.clone()
  }

  pub fn state(&self) -> State {
    self.state
  }
}
