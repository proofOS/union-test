#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    to_binary, Addr, Binary, Coins, Deps, DepsMut, Env, IbcQuery, MessageInfo, Order,
    PortIdResponse, Response, StdError, StdResult,
};
use cw2::set_contract_version;
use token_factory_api::TokenFactoryMsg;
use ucs01_relay_api::{
    protocol::{TransferInput, TransferProtocol},
    types::{NoExtension, TransferToken},
};

use crate::{
    error::ContractError,
    msg::{
        ChannelResponse, ConfigResponse, ExecuteMsg, InitMsg, ListChannelsResponse, MigrateMsg,
        PortResponse, QueryMsg, ReceivePhase1Msg, TransferMsg,
    },
    protocol::{Ics20Protocol, ProtocolCommon, Ucs01Protocol},
    state::{Config, ADMIN, CHANNEL_INFO, CHANNEL_STATE, CONFIG},
};

const CONTRACT_NAME: &str = "crates.io:ucs01-relay";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    mut deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InitMsg,
) -> Result<Response, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    let cfg = Config {
        default_timeout: msg.default_timeout,
    };
    CONFIG.save(deps.storage, &cfg)?;

    let admin = deps.api.addr_validate(&msg.gov_contract)?;
    ADMIN.set(deps.branch(), Some(admin))?;

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response<TokenFactoryMsg>, ContractError> {
    match msg {
        ExecuteMsg::Transfer(msg) => execute_transfer(deps, env, info, msg),
        ExecuteMsg::UpdateAdmin { admin } => {
            let admin = deps.api.addr_validate(&admin)?;
            Ok(ADMIN.execute_update_admin(deps, info, Some(admin))?)
        }
        ExecuteMsg::ReceivePhase1(msg) => execute_receive_phase1(deps, env, info, msg),
    }
}

pub fn execute_transfer(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: TransferMsg,
) -> Result<Response<TokenFactoryMsg>, ContractError> {
    let tokens: Vec<TransferToken> = Coins::try_from(info.funds.clone())
        .map_err(|_| StdError::generic_err("Couldn't decode funds to Coins"))?
        .into_vec()
        .into_iter()
        .map(Into::into)
        .collect();

    // At least one token must be transferred
    if tokens.is_empty() {
        return Err(ContractError::NoFunds {});
    }

    let channel_info = CHANNEL_INFO.load(deps.storage, &msg.channel)?;

    let config = CONFIG.load(deps.storage)?;

    let input = TransferInput {
        current_time: env.block.time,
        timeout_delta: config.default_timeout,
        sender: info.sender.clone(),
        receiver: msg.receiver,
        tokens,
    };

    match channel_info.protocol_version.as_str() {
        Ics20Protocol::VERSION => Ics20Protocol {
            common: ProtocolCommon {
                deps,
                env,
                info,
                channel: channel_info,
            },
        }
        .send(input, msg.memo),
        Ucs01Protocol::VERSION => Ucs01Protocol {
            common: ProtocolCommon {
                deps,
                env,
                info,
                channel: channel_info,
            },
        }
        .send(input, NoExtension),
        v => Err(ContractError::UnknownProtocol {
            channel_id: msg.channel,
            protocol_version: v.into(),
        }),
    }
}

pub fn execute_receive_phase1(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ReceivePhase1Msg,
) -> Result<Response<TokenFactoryMsg>, ContractError> {
    let channel_info = CHANNEL_INFO.load(deps.storage, &msg.channel)?;

    match channel_info.protocol_version.as_str() {
        Ics20Protocol::VERSION => Ics20Protocol {
            common: ProtocolCommon {
                deps,
                env,
                info,
                channel: channel_info,
            },
        }
        .receive_phase1(msg.raw_packet),
        Ucs01Protocol::VERSION => Ucs01Protocol {
            common: ProtocolCommon {
                deps,
                env,
                info,
                channel: channel_info,
            },
        }
        .receive_phase1(msg.raw_packet),
        v => Err(ContractError::UnknownProtocol {
            channel_id: msg.channel,
            protocol_version: v.into(),
        }),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(_: DepsMut, _: Env, _: MigrateMsg) -> Result<Response, ContractError> {
    Ok(Response::new())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Port {} => to_binary(&query_port(deps)?),
        QueryMsg::ListChannels {} => to_binary(&query_list(deps)?),
        QueryMsg::Channel { id } => to_binary(&query_channel(deps, id)?),
        QueryMsg::Config {} => to_binary(&query_config(deps)?),
        QueryMsg::Admin {} => to_binary(&ADMIN.query_admin(deps)?),
    }
}

fn query_port(deps: Deps) -> StdResult<PortResponse> {
    let query = IbcQuery::PortId {}.into();
    let PortIdResponse { port_id } = deps.querier.query(&query)?;
    Ok(PortResponse { port_id })
}

fn query_list(deps: Deps) -> StdResult<ListChannelsResponse> {
    let channels = CHANNEL_INFO
        .range_raw(deps.storage, None, None, Order::Ascending)
        .map(|r| r.map(|(_, v)| v))
        .collect::<StdResult<_>>()?;
    Ok(ListChannelsResponse { channels })
}

// make public for ibc tests
pub fn query_channel(deps: Deps, id: String) -> StdResult<ChannelResponse> {
    let info = CHANNEL_INFO.load(deps.storage, &id)?;
    let balances = CHANNEL_STATE
        .prefix(&id)
        .range(deps.storage, None, None, Order::Ascending)
        .map(|r| r.map(|(denom, v)| (denom.clone(), v.outstanding)))
        .collect::<StdResult<Vec<_>>>()?;
    Ok(ChannelResponse { info, balances })
}

fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let cfg = CONFIG.load(deps.storage)?;
    let admin = ADMIN.get(deps)?.unwrap_or_else(|| Addr::unchecked(""));
    let res = ConfigResponse {
        default_timeout: cfg.default_timeout,
        gov_contract: admin.into(),
    };
    Ok(res)
}