// SPDX-License-Identifier: MIT
pragma solidity 0.8.26;

struct PoolKey {
    address currency0;
    address currency1;
    uint24 fee;
    int24 tickSpacing;
    address hooks;
}

interface IERC20 {
    function balanceOf(address owner) external view returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function approve(address spender, uint256 amount) external returns (bool);
}

interface IWETH is IERC20 {
    function deposit() external payable;
    function withdraw(uint256 amount) external;
}

interface IV2Factory {
    function getPair(address tokenA, address tokenB) external view returns (address);
}

interface IV3Factory {
    function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
}

interface IV2Router {
    function factory() external view returns (address);
    function WETH() external view returns (address);
    function swapExactTokensForTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] calldata path,
        address to,
        uint256 deadline
    ) external returns (uint256[] memory);
}

interface IV3Router {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }
    function factory() external view returns (address);
    function WETH9() external view returns (address);
    function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256);
}

interface IPoolManager {
    struct SwapParams {
        bool zeroForOne;
        int256 amountSpecified;
        uint160 sqrtPriceLimitX96;
    }
    function unlock(bytes calldata data) external returns (bytes memory);
    function swap(PoolKey calldata key, SwapParams calldata params, bytes calldata hookData) external returns (int256);
    function sync(address currency) external;
    function settle() external payable returns (uint256);
    function take(address currency, address to, uint256 amount) external;
}

/// Executes caller-selected canonical V2/V3 and compatible V4 hops; quotes and discovery stay off-chain.
contract FeeRouter {
    enum Version {
        V2,
        V3,
        V4
    }

    struct Hop {
        Version version;
        address tokenIn;
        address tokenOut;
        address pool;
        uint24 fee;
        PoolKey key;
        bytes hookData;
    }

    struct SwapRequest {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint256 minimumAmountOut;
        address recipient;
        uint256 deadline;
    }

    uint256 public constant FEE_BPS = 100;
    uint256 private constant BPS = 10_000;
    uint160 private constant MIN_SQRT_PRICE = 4295128740;
    uint160 private constant MAX_SQRT_PRICE = 1461446703485210103287273052203988822378723970341;
    bytes32 private constant V2_INIT_CODE_HASH = 0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f;
    bytes32 private constant V3_INIT_CODE_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;

    address public immutable feeRecipient;
    address public immutable wrappedNative;
    IV2Router public immutable v2Router;
    IV3Router public immutable v3Router;
    address public immutable v2Factory;
    address public immutable v3Factory;
    IPoolManager public immutable poolManager;
    uint256 private entered = 1;
    bytes32 private pendingUnlock;

    error InvalidConfiguration();
    error InvalidRoute();
    error InvalidPool();
    error InvalidAmount();
    error Expired();
    error InsufficientOutput(uint256 actual, uint256 minimum);
    error TokenCallFailed(address token);
    error UnexpectedBalance(address token);
    error UnauthorizedCallback();
    error ReentrantCall();

    event SwapExecuted(
        address indexed sender,
        address indexed recipient,
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 fee,
        uint256 amountOut
    );

    constructor(
        address feeRecipient_,
        address wrappedNative_,
        address v2Router_,
        address v3Router_,
        address poolManager_
    ) {
        if (
            feeRecipient_ == address(0) || feeRecipient_ == address(this) || wrappedNative_.code.length == 0
                || v2Router_.code.length == 0 || v3Router_.code.length == 0 || poolManager_.code.length == 0
        ) revert InvalidConfiguration();
        feeRecipient = feeRecipient_;
        wrappedNative = wrappedNative_;
        v2Router = IV2Router(v2Router_);
        v3Router = IV3Router(v3Router_);
        poolManager = IPoolManager(poolManager_);
        if (v2Router.WETH() != wrappedNative_ || v3Router.WETH9() != wrappedNative_) revert InvalidConfiguration();
        v2Factory = v2Router.factory();
        v3Factory = v3Router.factory();
        if (v2Factory.code.length == 0 || v3Factory.code.length == 0) revert InvalidConfiguration();
    }

    receive() external payable {
        if (msg.sender != wrappedNative && msg.sender != address(poolManager)) revert UnauthorizedCallback();
    }

    modifier nonReentrant() {
        if (entered != 1) revert ReentrantCall();
        entered = 2;
        _;
        entered = 1;
    }

    /// The fee is floor(gross input / 100), even if a price boundary leaves some input unspent.
    /// Only standard, balance-preserving tokens are supported; no transfer-tax or rebase accounting.
    function swap(SwapRequest calldata request, Hop[] calldata hops)
        external
        payable
        nonReentrant
        returns (uint256 amountOut)
    {
        if (block.timestamp > request.deadline) revert Expired();
        if (hops.length == 0 || request.recipient == address(0) || request.recipient == address(this)) {
            revert InvalidRoute();
        }
        if (request.amountIn == 0) revert InvalidAmount();
        _collectInput(request.tokenIn, request.amountIn);
        uint256 fee = request.amountIn / (BPS / FEE_BPS);
        uint256 amount = request.amountIn - fee;
        address currency = request.tokenIn;
        for (uint256 i; i < hops.length; ++i) {
            _convert(currency, hops[i].tokenIn, amount);
            amount = _executeHop(hops[i], amount, request.deadline);
            currency = hops[i].tokenOut;
        }
        _convert(currency, request.tokenOut, amount);
        if (amount < request.minimumAmountOut) revert InsufficientOutput(amount, request.minimumAmountOut);
        uint256 recipientBefore =
            request.tokenOut == address(0) ? 0 : IERC20(request.tokenOut).balanceOf(request.recipient);
        _pay(request.tokenOut, request.recipient, amount);
        if (
            request.tokenOut != address(0)
                && IERC20(request.tokenOut).balanceOf(request.recipient) != recipientBefore + amount
        ) revert UnexpectedBalance(request.tokenOut);
        _pay(request.tokenIn, feeRecipient, fee);
        emit SwapExecuted(
            msg.sender, request.recipient, request.tokenIn, request.tokenOut, request.amountIn, fee, amount
        );
        return amount;
    }

    function _executeHop(Hop calldata hop, uint256 amount, uint256 deadline) private returns (uint256 output) {
        if (hop.tokenIn == hop.tokenOut || amount == 0) revert InvalidRoute();
        uint256 inputBefore = _balance(hop.tokenIn);
        uint256 outputBefore = _balance(hop.tokenOut);
        if (inputBefore < amount) revert UnexpectedBalance(hop.tokenIn);
        if (hop.version == Version.V2) _swapV2(hop, amount, deadline);
        else if (hop.version == Version.V3) _swapV3(hop, amount);
        else _swapV4(hop, amount);
        uint256 inputAfter = _balance(hop.tokenIn);
        uint256 outputAfter = _balance(hop.tokenOut);
        if (inputAfter > inputBefore || inputBefore - inputAfter > amount) revert UnexpectedBalance(hop.tokenIn);
        if (outputAfter <= outputBefore) revert UnexpectedBalance(hop.tokenOut);
        output = outputAfter - outputBefore;
        // Refund only this hop's unspent budget. Existing router balances and the fee are never route input.
        _pay(hop.tokenIn, msg.sender, amount - (inputBefore - inputAfter));
    }

    function _swapV2(Hop calldata hop, uint256 amount, uint256 deadline) private {
        (address token0, address token1) = _orderedTokens(hop);
        bytes32 salt = keccak256(abi.encodePacked(token0, token1));
        if (
            hop.pool != _poolAddress(v2Factory, salt, V2_INIT_CODE_HASH)
                || IV2Factory(v2Factory).getPair(token0, token1) != hop.pool
        ) revert InvalidPool();
        _approve(hop.tokenIn, address(v2Router), amount);
        address[] memory path = new address[](2);
        path[0] = hop.tokenIn;
        path[1] = hop.tokenOut;
        v2Router.swapExactTokensForTokens(amount, 0, path, address(this), deadline);
        _approve(hop.tokenIn, address(v2Router), 0);
    }

    function _swapV3(Hop calldata hop, uint256 amount) private {
        (address token0, address token1) = _orderedTokens(hop);
        bytes32 salt = keccak256(abi.encode(token0, token1, hop.fee));
        if (
            hop.pool != _poolAddress(v3Factory, salt, V3_INIT_CODE_HASH)
                || IV3Factory(v3Factory).getPool(token0, token1, hop.fee) != hop.pool
        ) revert InvalidPool();
        if (amount > uint256(type(int256).max)) revert InvalidAmount();
        _approve(hop.tokenIn, address(v3Router), amount);
        v3Router.exactInputSingle(
            IV3Router.ExactInputSingleParams(hop.tokenIn, hop.tokenOut, hop.fee, address(this), amount, 0, 0)
        );
        _approve(hop.tokenIn, address(v3Router), 0);
    }

    function _swapV4(Hop calldata hop, uint256 amount) private {
        if (hop.key.currency0 >= hop.key.currency1 || amount > uint256(type(int256).max)) revert InvalidPool();
        bool forward = hop.tokenIn == hop.key.currency0 && hop.tokenOut == hop.key.currency1;
        if (!forward && !(hop.tokenIn == hop.key.currency1 && hop.tokenOut == hop.key.currency0)) revert InvalidPool();
        bytes memory data = abi.encode(hop.key, forward, amount, hop.hookData);
        pendingUnlock = keccak256(data);
        poolManager.unlock(data);
        if (pendingUnlock != bytes32(0)) revert UnauthorizedCallback();
    }

    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        if (
            msg.sender != address(poolManager) || entered != 2 || pendingUnlock == bytes32(0)
                || keccak256(data) != pendingUnlock
        ) revert UnauthorizedCallback();
        pendingUnlock = bytes32(0);
        (PoolKey memory key, bool forward, uint256 amount, bytes memory hookData) =
            abi.decode(data, (PoolKey, bool, uint256, bytes));
        int256 delta = poolManager.swap(
            key, IPoolManager.SwapParams(forward, -int256(amount), forward ? MIN_SQRT_PRICE : MAX_SQRT_PRICE), hookData
        );
        int128 delta0 = int128(delta >> 128);
        int128 delta1 = int128(delta);
        int128 inputDelta = forward ? delta0 : delta1;
        int128 outputDelta = forward ? delta1 : delta0;
        if (inputDelta > 0 || outputDelta <= 0) revert InvalidAmount();
        uint256 spent = uint256(-int256(inputDelta));
        if (spent > amount) revert InvalidAmount();
        address input = forward ? key.currency0 : key.currency1;
        if (spent != 0) {
            poolManager.sync(input);
            if (input == address(0)) {
                if (poolManager.settle{value: spent}() != spent) revert UnexpectedBalance(input);
            } else {
                _pay(input, address(poolManager), spent);
                if (poolManager.settle() != spent) revert UnexpectedBalance(input);
            }
        }
        poolManager.take(forward ? key.currency1 : key.currency0, address(this), uint256(uint128(outputDelta)));
        return "";
    }

    function _collectInput(address token, uint256 amount) private {
        if (token == address(0)) {
            if (msg.value != amount) revert InvalidAmount();
        } else {
            if (msg.value != 0) revert InvalidAmount();
            uint256 beforeBalance = _balance(token);
            _tokenCall(token, abi.encodeCall(IERC20.transferFrom, (msg.sender, address(this), amount)));
            if (_balance(token) != beforeBalance + amount) revert UnexpectedBalance(token);
        }
    }

    function _convert(address from, address to, uint256 amount) private {
        if (from == to) return;
        if (from == address(0) && to == wrappedNative) IWETH(wrappedNative).deposit{value: amount}();
        else if (from == wrappedNative && to == address(0)) IWETH(wrappedNative).withdraw(amount);
        else revert InvalidRoute();
    }

    function _orderedTokens(Hop calldata hop) private pure returns (address token0, address token1) {
        if (hop.tokenIn == address(0) || hop.tokenOut == address(0)) revert InvalidPool();
        return hop.tokenIn < hop.tokenOut ? (hop.tokenIn, hop.tokenOut) : (hop.tokenOut, hop.tokenIn);
    }

    function _poolAddress(address factory, bytes32 salt, bytes32 initCodeHash) private pure returns (address) {
        return address(uint160(uint256(keccak256(abi.encodePacked(hex"ff", factory, salt, initCodeHash)))));
    }

    function _balance(address token) private view returns (uint256) {
        return token == address(0) ? address(this).balance : IERC20(token).balanceOf(address(this));
    }

    function _approve(address token, address spender, uint256 amount) private {
        _tokenCall(token, abi.encodeCall(IERC20.approve, (spender, 0)));
        if (amount != 0) _tokenCall(token, abi.encodeCall(IERC20.approve, (spender, amount)));
    }

    function _pay(address token, address to, uint256 amount) private {
        if (amount == 0) return;
        if (token == address(0)) {
            (bool success,) = to.call{value: amount}("");
            if (!success) revert TokenCallFailed(token);
        } else {
            _tokenCall(token, abi.encodeCall(IERC20.transfer, (to, amount)));
        }
    }

    function _tokenCall(address token, bytes memory data) private {
        (bool success, bytes memory result) = token.call(data);
        if (!success || token.code.length == 0 || (result.length != 0 && !abi.decode(result, (bool)))) {
            revert TokenCallFailed(token);
        }
    }
}
