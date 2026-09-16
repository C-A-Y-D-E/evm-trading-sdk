// SPDX-License-Identifier: MIT
pragma solidity 0.8.26;

import {FeeRouter, PoolKey as RouterPoolKey} from "../src/FeeRouter.sol";

interface ForkVm {
    function createSelectFork(string calldata url) external returns (uint256);
    function envString(string calldata name) external returns (string memory);
    function deal(address account, uint256 balance) external;
}

interface ForkToken {
    function deposit() external payable;
    function approve(address spender, uint256 amount) external returns (bool);
    function balanceOf(address owner) external view returns (uint256);
}

interface ForkV3 {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }
    function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256);
}

interface ForkPermit2 {
    function approve(address token, address spender, uint160 amount, uint48 expiration) external;
}

interface ForkUniversalRouter {
    function execute(bytes calldata commands, bytes[] calldata inputs, uint256 deadline) external payable;
}

interface ForkV3Quoter {
    struct QuoteParams {
        address tokenIn;
        address tokenOut;
        uint256 amountIn;
        uint24 fee;
        uint160 sqrtPriceLimitX96;
    }
    function quoteExactInputSingle(QuoteParams calldata params) external returns (uint256, uint160, uint32, uint256);
}

interface ForkV4Quoter {
    struct QuoteParams {
        RouterPoolKey poolKey;
        bool zeroForOne;
        uint128 exactAmount;
        bytes hookData;
    }
    function quoteExactInputSingle(QuoteParams calldata params) external returns (uint256, uint256);
}

contract RobinhoodForkTest {
    event log_named_uint(string key, uint256 value);
    ForkVm constant vm = ForkVm(address(uint160(uint256(keccak256("hevm cheat code")))));
    address constant WETH = 0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73;
    address constant NVDA = 0xd0601CE157Db5bdC3162BbaC2a2C8aF5320D9EEC;
    address constant AC = 0xfaD40755de679B262337f8D964D75CAc51F8C643;
    address constant V3 = 0xCaf681a66D020601342297493863E78C959E5cb2;
    address constant ROUTER = 0x8876789976dEcBfCbBbe364623C63652db8C0904;
    address constant PERMIT2 = 0x000000000022D473030F116dDEE9F6B43aC78BA3;

    struct PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    struct Swap {
        PoolKey poolKey;
        bool zeroForOne;
        uint128 amountIn;
        uint128 amountOutMinimum;
        uint256 minHopPriceX36;
        bytes hookData;
    }

    function setUp() public {
        vm.createSelectFork(vm.envString("ROBINHOOD_RPC_URL"));
        vm.deal(address(this), 1 ether);
    }

    function testDeployedV4RouterSettlesTheSavedPool() public {
        ForkToken(WETH).deposit{value: 0.04 ether}();
        ForkToken(WETH).approve(V3, 0.04 ether);
        uint256 nvda = ForkV3(V3)
            .exactInputSingle(ForkV3.ExactInputSingleParams(WETH, NVDA, 500, address(this), 0.04 ether, 1, 0));
        require(nvda > 0 && nvda <= type(uint128).max, "NVDA input");
        ForkToken(NVDA).approve(PERMIT2, nvda);
        ForkPermit2(PERMIT2).approve(NVDA, ROUTER, uint160(nvda), uint48(block.timestamp + 300));

        PoolKey memory key = PoolKey(NVDA, AC, 0, 200, 0xE5e702641Ea86F4ae6cC3cDaeD2B886f976Be044);
        bytes[] memory actions = new bytes[](3);
        actions[0] = abi.encode(Swap(key, true, uint128(nvda), 1, 0, ""));
        actions[1] = abi.encode(NVDA, nvda);
        actions[2] = abi.encode(AC, uint256(1));
        bytes[] memory inputs = new bytes[](1);
        inputs[0] = abi.encode(hex"060c0f", actions);
        uint256 beforeBalance = ForkToken(AC).balanceOf(address(this));
        ForkUniversalRouter(ROUTER).execute(hex"10", inputs, block.timestamp + 300);
        require(ForkToken(AC).balanceOf(address(this)) > beforeBalance, "AC output");
        require(ForkToken(NVDA).balanceOf(address(this)) == 0, "NVDA settled");
    }

    function testFeeRouterExecutesV3AndV4InOneCall() public {
        address feeRecipient = address(0xf33);
        FeeRouter feeRouter = new FeeRouter(
            feeRecipient,
            WETH,
            address(bytes20(hex"89e5db8b5aa49aa85ac63f691524311aeb649eba")),
            V3,
            address(bytes20(hex"8366a39cc670b4001a1121b8f6a443a643e40951"))
        );
        ForkToken(WETH).deposit{value: 0.04 ether}();
        ForkToken(WETH).approve(address(feeRouter), 0.04 ether);
        FeeRouter.Hop[] memory hops = new FeeRouter.Hop[](2);
        hops[0].version = FeeRouter.Version.V3;
        hops[0].tokenIn = WETH;
        hops[0].tokenOut = NVDA;
        hops[0].pool = address(bytes20(hex"62ab521f71431f78ac374cdbadc6cda3c8916b6c"));
        hops[0].fee = 500;
        hops[1].version = FeeRouter.Version.V4;
        hops[1].tokenIn = NVDA;
        hops[1].tokenOut = AC;
        hops[1].key = RouterPoolKey(NVDA, AC, 0, 200, 0xE5e702641Ea86F4ae6cC3cDaeD2B886f976Be044);
        uint256 beforeBalance = ForkToken(AC).balanceOf(address(this));
        uint256 output =
            feeRouter.swap(FeeRouter.SwapRequest(WETH, AC, 0.04 ether, 1, address(this), block.timestamp + 300), hops);
        require(output > 0 && ForkToken(AC).balanceOf(address(this)) == beforeBalance + output, "AC received");
        require(ForkToken(WETH).balanceOf(feeRecipient) == 0.0004 ether, "one percent fee");
        require(ForkToken(NVDA).balanceOf(address(feeRouter)) == 0, "no intermediate residue");
        require(ForkToken(WETH).balanceOf(address(feeRouter)) == 0, "no input residue");
    }

    function testFeeRouterExecutesTtwoGtaviPool() public {
        address ttwo = address(bytes20(hex"5e81213613b6b86eab4c6c50d718d34359459786"));
        address gtavi = address(bytes20(hex"a2bc347ebda4b5c781d29a3d39726693e2be1e18"));
        RouterPoolKey memory key =
            RouterPoolKey(ttwo, gtavi, 0x800000, 8, address(bytes20(hex"4e3468951d49f2eea976ed0d6e75ffcb44a9a544")));
        require(
            keccak256(abi.encode(key)) == 0xcab486171869c295b45e22bf7457be9436af5a92bc6fa2302d1ec742b43660b4,
            "selected pool ID"
        );
        (uint256 ttwoQuote,,,) = ForkV3Quoter(address(bytes20(hex"33e885ed0ec9bf04ecfb19341582aadcb4c8a9e7")))
            .quoteExactInputSingle(ForkV3Quoter.QuoteParams(WETH, ttwo, 0.0396 ether, 3000, 0));
        require(ttwoQuote > 0 && ttwoQuote <= type(uint128).max, "TTWO quote");
        (uint256 expectedOutput,) = ForkV4Quoter(address(bytes20(hex"8dc178efb8111bb0973dd9d722ebeff267c98f94")))
            .quoteExactInputSingle(ForkV4Quoter.QuoteParams(key, true, uint128(ttwoQuote), ""));
        require(expectedOutput > 0, "GTAVI quote");

        address feeRecipient = address(0xf33);
        FeeRouter feeRouter = new FeeRouter(
            feeRecipient,
            WETH,
            address(bytes20(hex"89e5db8b5aa49aa85ac63f691524311aeb649eba")),
            V3,
            address(bytes20(hex"8366a39cc670b4001a1121b8f6a443a643e40951"))
        );
        ForkToken(WETH).deposit{value: 0.04 ether}();
        ForkToken(WETH).approve(address(feeRouter), 0.04 ether);
        FeeRouter.Hop[] memory hops = new FeeRouter.Hop[](2);
        hops[0].version = FeeRouter.Version.V3;
        hops[0].tokenIn = WETH;
        hops[0].tokenOut = ttwo;
        hops[0].pool = address(bytes20(hex"e69fe23b708362ef5817048f16e4171e0831341c"));
        hops[0].fee = 3000;
        hops[1].version = FeeRouter.Version.V4;
        hops[1].tokenIn = ttwo;
        hops[1].tokenOut = gtavi;
        hops[1].key = key;
        uint256 beforeBalance = ForkToken(gtavi).balanceOf(address(this));
        uint256 output = feeRouter.swap(
            FeeRouter.SwapRequest(
                WETH, gtavi, 0.04 ether, expectedOutput * 99 / 100, address(this), block.timestamp + 300
            ),
            hops
        );
        require(output > 0 && ForkToken(gtavi).balanceOf(address(this)) == beforeBalance + output, "GTAVI received");
        require(ForkToken(WETH).balanceOf(feeRecipient) == 0.0004 ether, "one percent fee");
        require(ForkToken(ttwo).balanceOf(address(feeRouter)) == 0, "no intermediate residue");
        require(ForkToken(WETH).balanceOf(address(feeRouter)) == 0, "no input residue");
        emit log_named_uint("Fork block", block.number);
        emit log_named_uint("TTWO quote (18 decimals)", ttwoQuote);
        emit log_named_uint("GTAVI quote (18 decimals)", expectedOutput);
        emit log_named_uint("GTAVI received (18 decimals)", output);
    }
}
