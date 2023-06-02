pragma solidity ^0.8.18;

import "./core/02-client/ILightClient.sol";
import "./core/02-client/IBCHeight.sol";
import "./proto/ibc/core/client/v1/client.sol";
import "./proto/ibc/lightclients/tendermint/v1/tendermint.sol";
import "./proto/cosmos/ics23/v1/proofs.sol";
import "./proto/tendermint/types/types.sol";
import "./proto/tendermint/types/canonical.sol";
import "./proto/union/ibc/lightclients/cometbls/v1/cometbls.sol";
import "./proto/ibc/lightclients/wasm/v1/wasm.sol";
import "./lib/CometblsHelp.sol";

contract Glue {
    function typesTelescope(
      UnionIbcLightclientsCometblsV1ClientState.Data memory,
      UnionIbcLightclientsCometblsV1ConsensusState.Data memory,
      TendermintTypesHeader.Data memory,
      TendermintTypesCommit.Data memory,
      IbcCoreClientV1Height.Data memory,
      OptimizedConsensusState memory,
      ProcessedMoment memory,
      TendermintTypesCanonicalVote.Data memory
    ) public pure {
    }
}