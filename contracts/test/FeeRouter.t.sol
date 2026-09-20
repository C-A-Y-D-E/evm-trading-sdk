// SPDX-License-Identifier: MIT
pragma solidity 0.8.26;

import {FeeRouter, PoolKey, IERC20, IV3Router, IPoolManager} from "../src/FeeRouter.sol";
import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";

interface Vm {
    function deal(address account, uint256 balance) external;
    function expectRevert(bytes4 selector) external;
    function expectRevert(bytes calldata data) external;
    function expectRevert() external;
    function warp(uint256 timestamp) external;
    function prank(address sender) external;
    function expectEmit(bool topic1, bool topic2, bool topic3, bool data, address emitter) external;
    function mockCall(address callee, bytes calldata data, bytes calldata result) external;
    function mockCallRevert(address callee, bytes calldata data, bytes calldata reason) external;
    function clearMockedCalls() external;
}

contract MockToken {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    bool public tax;

    function mint(address owner, uint256 amount) external {
        balanceOf[owner] += amount;
    }

    function setTax(bool enabled) external {
        tax = enabled;
    }

    function deposit() external payable {
        balanceOf[msg.sender] += msg.value;
    }

    function withdraw(uint256 amount) external {
        balanceOf[msg.sender] -= amount;
        (bool ok,) = msg.sender.call{value: amount}("");
        require(ok);
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        allowance[from][msg.sender] -= amount;
        _transfer(from, to, amount);
        return true;
    }

    function _transfer(address from, address to, uint256 amount) private {
        balanceOf[from] -= amount;
        balanceOf[to] += tax ? amount - amount / 10 : amount;
    }
}

contract MockFactory {
    bytes32 constant V2_HASH = 0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f;
    bytes32 constant V3_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;

    function getPair(address a, address b) external view returns (address) {
        (a, b) = a < b ? (a, b) : (b, a);
        return _address(keccak256(abi.encodePacked(a, b)), V2_HASH);
    }

    function getPool(address a, address b, uint24 fee) external view returns (address) {
        (a, b) = a < b ? (a, b) : (b, a);
        return _address(keccak256(abi.encode(a, b, fee)), V3_HASH);
    }

    function _address(bytes32 salt, bytes32 hash) private view returns (address) {
        return address(uint160(uint256(keccak256(abi.encodePacked(hex"ff", address(this), salt, hash)))));
    }
}

contract MockDex {
    address public immutable factory;
    address public immutable WETH;
    uint256 public fillBps = 10_000;

    constructor(address factory_, address weth_) {
        factory = factory_;
        WETH = weth_;
    }

    function WETH9() external view returns (address) {
        return WETH;
    }

    function setFill(uint256 fill) external {
        fillBps = fill;
    }

    function exactInputSingle(IV3Router.ExactInputSingleParams calldata params) external payable returns (uint256) {
        return _swap(params.tokenIn, params.tokenOut, params.amountIn, params.recipient);
    }

    function swapExactTokensForTokens(uint256 amount, uint256, address[] calldata path, address recipient, uint256)
        external
        returns (uint256[] memory amounts)
    {
        amounts = new uint256[](2);
        amounts[0] = amount;
        amounts[1] = _swap(path[0], path[1], amount, recipient);
    }

    function _swap(address input, address output, uint256 amount, address recipient) private returns (uint256) {
        uint256 spent = amount * fillBps / 10_000;
        IERC20(input).transferFrom(msg.sender, address(this), spent);
        MockToken(output).mint(recipient, spent * 2);
        return spent * 2;
    }
}

contract MockManager {
    address private currency;
    uint256 private synced;
    uint256 private debt;
    bool private unlocked;

    function unlock(bytes calldata data) external returns (bytes memory result) {
        require(!unlocked);
        unlocked = true;
        result = FeeRouter(payable(msg.sender)).unlockCallback(data);
        require(debt == 0);
        unlocked = false;
    }

    function swap(PoolKey calldata key, IPoolManager.SwapParams calldata params, bytes calldata hookData)
        external
        returns (int256)
    {
        require(unlocked && hookData.length == 0, "hook rejected");
        debt = uint256(-params.amountSpecified);
        require(debt < uint256(uint128(type(int128).max)) / 2);
        int128 input = -int128(int256(debt));
        int128 output = int128(int256(debt * 2));
        int128 delta0 = params.zeroForOne ? input : output;
        int128 delta1 = params.zeroForOne ? output : input;
        address outputToken = params.zeroForOne ? key.currency1 : key.currency0;
        if (outputToken != address(0)) MockToken(outputToken).mint(address(this), debt * 2);
        return (int256(delta0) << 128) | int256(uint256(uint128(delta1)));
    }

    function sync(address token) external {
        currency = token;
        synced = token == address(0) ? 0 : IERC20(token).balanceOf(address(this));
    }

    function settle() external payable returns (uint256 paid) {
        paid = currency == address(0) ? msg.value : IERC20(currency).balanceOf(address(this)) - synced;
        require(paid == debt);
        debt = 0;
    }

    function take(address token, address to, uint256 amount) external {
        if (token == address(0)) {
            (bool ok,) = to.call{value: amount}("");
            require(ok);
        } else {
            IERC20(token).transfer(to, amount);
        }
    }
}

contract ReenteringRecipient {
    FeeRouter private router;
    bool public blocked;

    constructor(FeeRouter router_) {
        router = router_;
    }

    receive() external payable {
        FeeRouter.Hop[] memory hops = new FeeRouter.Hop[](0);
        FeeRouter.SwapRequest memory request;
        (bool success, bytes memory reason) = address(router).call(abi.encodeCall(FeeRouter.swap, (request, hops)));
        blocked = !success && keccak256(reason) == keccak256(abi.encodeWithSelector(FeeRouter.ReentrantCall.selector));
        require(blocked);
    }
}

contract NoReturnToken {
    mapping(address => uint256) public balanceOf;

    constructor(address holder, uint256 amount) {
        balanceOf[holder] = amount;
    }

    function transfer(address to, uint256 amount) external {
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
    }
}

contract ReenteringFeeRecipient {
    bool public rescueBlocked;
    bool public swapBlocked;
    bool public feeChangeBlocked;

    receive() external payable {
        FeeRouter router = FeeRouter(payable(msg.sender));
        (bool success, bytes memory reason) = address(router).call(abi.encodeCall(FeeRouter.rescue, (address(0), 1)));
        rescueBlocked =
            !success && keccak256(reason) == keccak256(abi.encodeWithSelector(FeeRouter.ReentrantCall.selector));
        FeeRouter.Hop[] memory hops = new FeeRouter.Hop[](0);
        FeeRouter.SwapRequest memory request;
        (success, reason) = address(router).call(abi.encodeCall(FeeRouter.swap, (request, hops)));
        swapBlocked =
            !success && keccak256(reason) == keccak256(abi.encodeWithSelector(FeeRouter.ReentrantCall.selector));
        require(rescueBlocked && swapBlocked, "shared reentrancy lock");
        if (router.owner() == address(this)) {
            (success, reason) = address(router).call(abi.encodeCall(FeeRouter.setFeeBps, (200)));
            feeChangeBlocked =
                !success && keccak256(reason) == keccak256(abi.encodeWithSelector(FeeRouter.ReentrantCall.selector));
            require(feeChangeBlocked, "fee change during callback");
        }
    }
}

contract FeeRouterTest {
    event FeeUpdated(uint256 previousFeeBps, uint256 newFeeBps);
    event FundsRescued(address indexed token, address indexed recipient, uint256 amount);

    Vm constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));
    address constant FEE_RECIPIENT = address(0xf33);
    address constant RECIPIENT = address(0xbeef);
    MockToken weth;
    MockToken middle;
    MockToken output;
    MockFactory factory;
    MockDex dex;
    MockManager manager;
    FeeRouter router;

    function setUp() public {
        weth = new MockToken();
        middle = new MockToken();
        output = new MockToken();
        factory = new MockFactory();
        dex = new MockDex(address(factory), address(weth));
        manager = new MockManager();
        router = _deployRouter(FEE_RECIPIENT);
        weth.mint(address(this), 1 ether);
        weth.approve(address(router), type(uint256).max);
        vm.deal(address(this), 10 ether);
        vm.deal(address(manager), 10 ether);
    }

    function testV3AndV4ChargeOneFeeAndLeaveNoIntermediateBalance() public {
        FeeRouter.Hop[] memory hops = _route();
        uint256 amount = router.swap(_request(10000, 39600), hops);
        require(amount == 39600 && output.balanceOf(RECIPIENT) == 39600);
        require(weth.balanceOf(FEE_RECIPIENT) == 100);
        require(middle.balanceOf(address(router)) == 0 && weth.balanceOf(address(router)) == 0);
        require(weth.allowance(address(router), address(dex)) == 0);
    }

    function testV2DirectSwapUsesTheSelectedPool() public {
        FeeRouter.Hop[] memory hops = new FeeRouter.Hop[](1);
        hops[0] = _hop(FeeRouter.Version.V2, address(weth), address(output));
        require(router.swap(_request(10000, 19800), hops) == 19800);
    }

    function testWrongPoolCannotSilentlyRouteSomewhereElse() public {
        FeeRouter.Hop[] memory hops = _route();
        hops[0].pool = address(123);
        vm.expectRevert(FeeRouter.InvalidPool.selector);
        router.swap(_request(10000, 1), hops);
    }

    function testNativeInputWrapsAndPaysTheFeeInEth() public {
        FeeRouter.SwapRequest memory request = _request(10000, 39600);
        request.tokenIn = address(0);
        router.swap{value: 10000}(request, _route());
        require(FEE_RECIPIENT.balance == 100 && output.balanceOf(RECIPIENT) == 39600);
        require(address(router).balance == 0);
    }

    function testNativeV4PoolSettlesEth() public {
        FeeRouter.Hop[] memory hops = new FeeRouter.Hop[](1);
        hops[0] = _hop(FeeRouter.Version.V4, address(0), address(output));
        FeeRouter.SwapRequest memory request = _request(10000, 19800);
        request.tokenIn = address(0);
        require(router.swap{value: 10000}(request, hops) == 19800);
    }

    function testNativeOutputIsPaidToTheRecipient() public {
        FeeRouter.Hop[] memory hops = new FeeRouter.Hop[](1);
        hops[0] = _hop(FeeRouter.Version.V4, address(weth), address(0));
        FeeRouter.SwapRequest memory request = _request(10000, 19800);
        request.tokenOut = address(0);
        router.swap(request, hops);
        require(RECIPIENT.balance == 19800);
    }

    function testRecipientCannotReenterDuringNativePayment() public {
        ReenteringRecipient recipient = new ReenteringRecipient(router);
        FeeRouter.Hop[] memory hops = new FeeRouter.Hop[](1);
        hops[0] = _hop(FeeRouter.Version.V4, address(weth), address(0));
        FeeRouter.SwapRequest memory request = _request(10000, 19800);
        request.tokenOut = address(0);
        request.recipient = address(recipient);
        router.swap(request, hops);
        require(recipient.blocked());
    }

    function testARepeatedCurrencyDoesNotSpendTheReservedFee() public {
        FeeRouter.Hop[] memory hops = _route();
        hops[1] = _hop(FeeRouter.Version.V4, address(middle), address(weth));
        FeeRouter.SwapRequest memory request = _request(10000, 39600);
        request.tokenOut = address(weth);
        router.swap(request, hops);
        require(weth.balanceOf(RECIPIENT) == 39600 && weth.balanceOf(FEE_RECIPIENT) == 100);
        require(weth.balanceOf(address(router)) == 0);
    }

    function testUnspentInputIsRefundedAndApprovalCleared() public {
        dex.setFill(5000);
        uint256 beforeBalance = weth.balanceOf(address(this));
        require(router.swap(_request(10000, 19800), _route()) == 19800);
        require(beforeBalance - weth.balanceOf(address(this)) == 5050);
        require(weth.balanceOf(FEE_RECIPIENT) == 100);
        require(weth.allowance(address(router), address(dex)) == 0);
    }

    function testLastHopRevertRestoresInputAndFee() public {
        FeeRouter.Hop[] memory hops = _route();
        hops[1].hookData = hex"ff";
        uint256 beforeBalance = weth.balanceOf(address(this));
        vm.expectRevert();
        router.swap(_request(10000, 1), hops);
        require(weth.balanceOf(address(this)) == beforeBalance);
        require(weth.balanceOf(FEE_RECIPIENT) == 0 && middle.balanceOf(address(router)) == 0);
    }

    function testMinimumOutputRevertsTheWholeRoute() public {
        FeeRouter.Hop[] memory hops = _route();
        vm.expectRevert();
        router.swap(_request(10000, 39601), hops);
        require(output.balanceOf(RECIPIENT) == 0 && weth.balanceOf(FEE_RECIPIENT) == 0);
    }

    function testExpiredTradesAndUnexpectedEthAreRejected() public {
        FeeRouter.Hop[] memory hops = _route();
        FeeRouter.SwapRequest memory request = _request(10000, 1);
        request.deadline = block.timestamp;
        vm.warp(block.timestamp + 1);
        vm.expectRevert(FeeRouter.Expired.selector);
        router.swap(request, hops);
        vm.expectRevert(FeeRouter.InvalidAmount.selector);
        router.swap{value: 1}(_request(10000, 1), hops);
    }

    function testCallbacksOutsideAnActiveManagerUnlockAreRejected() public {
        vm.expectRevert(FeeRouter.UnauthorizedCallback.selector);
        router.unlockCallback("");
    }

    function testDisconnectedRoutesAreRejected() public {
        FeeRouter.Hop[] memory hops = _route();
        hops[1].tokenIn = address(weth);
        vm.expectRevert(FeeRouter.InvalidRoute.selector);
        router.swap(_request(10000, 1), hops);
    }

    function testTransferTaxInputIsRejected() public {
        FeeRouter.Hop[] memory hops = _route();
        weth.setTax(true);
        vm.expectRevert(abi.encodeWithSelector(FeeRouter.UnexpectedBalance.selector, address(weth)));
        router.swap(_request(10000, 1), hops);
    }

    function testTransferTaxOutputCannotUndercutTheMinimum() public {
        FeeRouter.Hop[] memory hops = _route();
        output.setTax(true);
        vm.expectRevert(abi.encodeWithSelector(FeeRouter.UnexpectedBalance.selector, address(output)));
        router.swap(_request(10000, 1), hops);
    }

    function testFuzzTheFeeRoundsDownAndExistingBalancesStayUntouched(uint96 rawAmount, uint96 dust) public {
        uint256 amount = uint256(rawAmount) % 1e17 + 1;
        weth.mint(address(router), dust);
        middle.mint(address(router), dust);
        output.mint(address(router), dust);
        router.swap(_request(amount, 1), _route());
        require(weth.balanceOf(FEE_RECIPIENT) == amount / 100);
        require(output.balanceOf(RECIPIENT) == (amount - amount / 100) * 4);
        require(
            weth.balanceOf(address(router)) == dust && middle.balanceOf(address(router)) == dust
                && output.balanceOf(address(router)) == dust
        );
    }

    function testOnlyFeeRecipientCanRescueTokensOrEth() public {
        weth.mint(address(router), 100);
        vm.deal(address(router), 100);
        vm.expectRevert(FeeRouter.UnauthorizedRescue.selector);
        router.rescue(address(weth), 100);
        vm.expectRevert(FeeRouter.UnauthorizedRescue.selector);
        router.rescue(address(0), 100);
        require(weth.balanceOf(address(router)) == 100 && address(router).balance == 100);
    }

    function testFuzzRescuePaysOnlyTheRequestedTokens(uint96 rawAmount, uint96 remainder) public {
        uint256 amount = uint256(rawAmount) + 1;
        weth.mint(address(router), amount + remainder);
        middle.mint(address(router), 123);
        vm.expectEmit(true, true, false, true, address(router));
        emit FundsRescued(address(weth), FEE_RECIPIENT, amount);
        vm.prank(FEE_RECIPIENT);
        router.rescue(address(weth), amount);
        require(weth.balanceOf(FEE_RECIPIENT) == amount);
        require(weth.balanceOf(address(router)) == remainder && middle.balanceOf(address(router)) == 123);
    }

    function testRescuePaysForcedEthAndCanRunAgain() public {
        vm.deal(address(router), 1 ether);
        vm.expectEmit(true, true, false, true, address(router));
        emit FundsRescued(address(0), FEE_RECIPIENT, 0.4 ether);
        vm.prank(FEE_RECIPIENT);
        router.rescue(address(0), 0.4 ether);
        require(FEE_RECIPIENT.balance == 0.4 ether && address(router).balance == 0.6 ether);
        vm.prank(FEE_RECIPIENT);
        router.rescue(address(0), 0.6 ether);
        require(FEE_RECIPIENT.balance == 1 ether && address(router).balance == 0);
    }

    function testRescueRejectsZeroAndExcessiveAmounts() public {
        weth.mint(address(router), 10);
        vm.deal(address(router), 10);
        vm.expectRevert(FeeRouter.InvalidAmount.selector);
        vm.prank(FEE_RECIPIENT);
        router.rescue(address(weth), 0);
        vm.expectRevert(FeeRouter.InvalidAmount.selector);
        vm.prank(FEE_RECIPIENT);
        router.rescue(address(weth), 11);
        vm.expectRevert(FeeRouter.InvalidAmount.selector);
        vm.prank(FEE_RECIPIENT);
        router.rescue(address(0), 11);
        require(weth.balanceOf(address(router)) == 10 && address(router).balance == 10);
    }

    function testRescueAcceptsTokensWithoutReturnValues() public {
        NoReturnToken token = new NoReturnToken(address(router), 100);
        vm.prank(FEE_RECIPIENT);
        router.rescue(address(token), 100);
        require(token.balanceOf(FEE_RECIPIENT) == 100 && token.balanceOf(address(router)) == 0);
    }

    function testRejectedTokenRecoveryKeepsFundsAndAllowsRetry() public {
        weth.mint(address(router), 100);
        bytes memory transfer = abi.encodeCall(IERC20.transfer, (FEE_RECIPIENT, 100));
        vm.mockCall(address(weth), transfer, abi.encode(false));
        vm.expectRevert(abi.encodeWithSelector(FeeRouter.TokenCallFailed.selector, address(weth)));
        vm.prank(FEE_RECIPIENT);
        router.rescue(address(weth), 100);
        require(weth.balanceOf(address(router)) == 100 && weth.balanceOf(FEE_RECIPIENT) == 0);
        vm.clearMockedCalls();
        vm.mockCallRevert(address(weth), transfer, hex"12345678");
        vm.expectRevert(abi.encodeWithSelector(FeeRouter.TokenCallFailed.selector, address(weth)));
        vm.prank(FEE_RECIPIENT);
        router.rescue(address(weth), 100);
        vm.clearMockedCalls();
        vm.prank(FEE_RECIPIENT);
        router.rescue(address(weth), 100);
        require(weth.balanceOf(address(router)) == 0 && weth.balanceOf(FEE_RECIPIENT) == 100);
    }

    function testRejectedEthRecoveryKeepsTheBalance() public {
        FeeRouter recoveryRouter = _deployRouter(address(output));
        vm.deal(address(recoveryRouter), 100);
        vm.expectRevert(abi.encodeWithSelector(FeeRouter.TokenCallFailed.selector, address(0)));
        vm.prank(address(output));
        recoveryRouter.rescue(address(0), 100);
        require(address(recoveryRouter).balance == 100 && address(output).balance == 0);
    }

    function testFeeRecipientCannotReenterDuringRescue() public {
        ReenteringFeeRecipient recipient = new ReenteringFeeRecipient();
        FeeRouter recoveryRouter = _deployRouter(address(recipient));
        vm.deal(address(recoveryRouter), 100);
        vm.prank(address(recipient));
        recoveryRouter.rescue(address(0), 40);
        require(recipient.rescueBlocked() && recipient.swapBlocked());
        require(address(recoveryRouter).balance == 60 && address(recipient).balance == 40);
    }

    function testFeeRecipientCannotRescueDuringSwap() public {
        ReenteringFeeRecipient recipient = new ReenteringFeeRecipient();
        FeeRouter recoveryRouter = _deployRouter(address(recipient));
        recoveryRouter.transferOwnership(address(recipient));
        vm.prank(address(recipient));
        recoveryRouter.acceptOwnership();
        vm.deal(address(recoveryRouter), 123);
        FeeRouter.SwapRequest memory request = _request(10000, 39600);
        request.tokenIn = address(0);
        recoveryRouter.swap{value: 10000}(request, _route());
        require(recipient.rescueBlocked() && recipient.swapBlocked() && recipient.feeChangeBlocked());
        require(address(recoveryRouter).balance == 123 && address(recipient).balance == 100);
        require(output.balanceOf(RECIPIENT) == 39600);
    }

    function testConstructorSetsConfigurationAndDefaultFee() public {
        weth.mint(address(router), 123);
        vm.deal(address(router), 456);
        require(router.FEE_BPS() == 100);
        require(router.owner() == address(this) && router.feeRecipient() == FEE_RECIPIENT);
        require(router.wrappedNative() == address(weth) && address(router.poolManager()) == address(manager));
        require(address(router.v2Router()) == address(dex) && address(router.v3Router()) == address(dex));
        require(router.v2Factory() == address(factory) && router.v3Factory() == address(factory));
        require(router.swap(_request(10000, 39600), _route()) == 39600);
        require(weth.balanceOf(address(router)) == 123 && address(router).balance == 456);
        vm.prank(FEE_RECIPIENT);
        router.rescue(address(weth), 123);
        require(weth.balanceOf(FEE_RECIPIENT) == 223);
    }

    function testConstructorRejectsInvalidConfiguration() public {
        vm.expectRevert(FeeRouter.InvalidConfiguration.selector);
        _deployRouter(address(0));
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableInvalidOwner.selector, address(0)));
        new FeeRouter(address(0), FEE_RECIPIENT, address(weth), address(dex), address(dex), address(manager));
        vm.expectRevert(FeeRouter.InvalidConfiguration.selector);
        new FeeRouter(address(this), FEE_RECIPIENT, address(output), address(dex), address(dex), address(manager));
    }

    function testOwnershipRequiresAcceptance() public {
        router.transferOwnership(RECIPIENT);
        require(router.owner() == address(this) && router.pendingOwner() == RECIPIENT);
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, FEE_RECIPIENT));
        vm.prank(FEE_RECIPIENT);
        router.acceptOwnership();
        vm.prank(RECIPIENT);
        router.acceptOwnership();
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, address(this)));
        router.setFeeBps(200);
        vm.prank(RECIPIENT);
        router.setFeeBps(200);
        require(router.owner() == RECIPIENT && router.FEE_BPS() == 200);
    }

    function testOwnerCannotChangeFeeDuringRecovery() public {
        ReenteringFeeRecipient recipient = new ReenteringFeeRecipient();
        FeeRouter recoveryRouter = _deployRouter(address(recipient));
        recoveryRouter.transferOwnership(address(recipient));
        vm.prank(address(recipient));
        recoveryRouter.acceptOwnership();
        vm.deal(address(recoveryRouter), 100);
        vm.prank(address(recipient));
        recoveryRouter.rescue(address(0), 40);
        require(recipient.feeChangeBlocked() && address(recoveryRouter).balance == 60);
    }

    function testOnlyOwnerCanSetTheFee() public {
        vm.expectRevert(abi.encodeWithSelector(Ownable.OwnableUnauthorizedAccount.selector, FEE_RECIPIENT));
        vm.prank(FEE_RECIPIENT);
        router.setFeeBps(200);
        require(router.FEE_BPS() == 100);
        vm.expectEmit(false, false, false, true, address(router));
        emit FeeUpdated(100, 333);
        router.setFeeBps(333);
        require(router.FEE_BPS() == 333);
    }

    function testZeroFeeRoutesTheWholeInput() public {
        router.setFeeBps(0);
        require(router.FEE_BPS() == 0);
        require(router.swapWithFee(_request(10000, 40000), _route(), 0) == 40000);
        require(weth.balanceOf(FEE_RECIPIENT) == 0);
    }

    function testFeeChangeRejectsAnOldQuoteBeforeTakingInput() public {
        router.setFeeBps(200);
        FeeRouter.Hop[] memory hops = _route();
        uint256 beforeBalance = weth.balanceOf(address(this));
        vm.expectRevert(abi.encodeWithSelector(FeeRouter.FeeChanged.selector, 100, 200));
        router.swapWithFee(_request(10000, 1), hops, 100);
        require(weth.balanceOf(address(this)) == beforeBalance && weth.balanceOf(FEE_RECIPIENT) == 0);
        require(router.swapWithFee(_request(10000, 39200), _route(), 200) == 39200);
    }

    function testFeeBoundsAndFullInputFee() public {
        FeeRouter.Hop[] memory hops = _route();
        vm.expectRevert(abi.encodeWithSelector(FeeRouter.InvalidFeeBps.selector, 10001));
        router.setFeeBps(10001);
        router.setFeeBps(10000);
        uint256 beforeBalance = weth.balanceOf(address(this));
        vm.expectRevert(FeeRouter.InvalidAmount.selector);
        router.swapWithFee(_request(10000, 1), hops, 10000);
        require(weth.balanceOf(address(this)) == beforeBalance);
    }

    function testNativeSwapsUseTheConfiguredFee() public {
        router.setFeeBps(333);
        FeeRouter.SwapRequest memory request = _request(10001, 38672);
        request.tokenIn = address(0);
        require(router.swapWithFee{value: 10001}(request, _route(), 333) == 38672);
        require(FEE_RECIPIENT.balance == 333);
    }

    function testFuzzConfiguredFeesRoundDownOnce(uint96 rawAmount, uint16 rawFee) public {
        uint256 amount = uint256(rawAmount) % 1e17 + 1;
        uint256 feeBps = uint256(rawFee) % 10000;
        uint256 fee = amount * feeBps / 10000;
        router.setFeeBps(feeBps);
        require(router.swapWithFee(_request(amount, 1), _route(), feeBps) == (amount - fee) * 4);
        require(weth.balanceOf(FEE_RECIPIENT) == fee);
    }

    function _deployRouter(address feeRecipient) private returns (FeeRouter) {
        return new FeeRouter(address(this), feeRecipient, address(weth), address(dex), address(dex), address(manager));
    }

    function _route() private view returns (FeeRouter.Hop[] memory hops) {
        hops = new FeeRouter.Hop[](2);
        hops[0] = _hop(FeeRouter.Version.V3, address(weth), address(middle));
        hops[1] = _hop(FeeRouter.Version.V4, address(middle), address(output));
    }

    function _hop(FeeRouter.Version version, address input, address tokenOut)
        private
        view
        returns (FeeRouter.Hop memory hop)
    {
        hop.version = version;
        hop.tokenIn = input;
        hop.tokenOut = tokenOut;
        if (version == FeeRouter.Version.V2) {
            hop.pool = factory.getPair(input, tokenOut);
        } else if (version == FeeRouter.Version.V3) {
            hop.fee = 500;
            hop.pool = factory.getPool(input, tokenOut, 500);
        } else {
            (address a, address b) = input < tokenOut ? (input, tokenOut) : (tokenOut, input);
            hop.key = PoolKey(a, b, 3000, 60, address(0));
        }
    }

    function _request(uint256 amount, uint256 minimum) private view returns (FeeRouter.SwapRequest memory) {
        return FeeRouter.SwapRequest(address(weth), address(output), amount, minimum, RECIPIENT, block.timestamp + 300);
    }
}
