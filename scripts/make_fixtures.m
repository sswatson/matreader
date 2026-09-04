function make_fixtures(outdir)
% Generate .mat fixtures covering the MATLAB type zoo in v6, v7 and v7.3.
rng(0);
scalarDouble = pi;
rowVec = 1:10;
colVec = (1:5)';
matrix = magic(6);
wideMatrix = reshape(1:(4*50), 4, 50);
tallMatrix = reshape(1:(3000*3), 3000, 3);
int32Mat = int32(reshape(1:12, 3, 4));
uint8Mat = uint8(magic(4));
int16Neg = int16([-3 -2 -1 0 1 2 3]);
int64Vec = int64([-5 0 5 9007199254740993]);
uint64Max = uint64(18446744073709551615);
singleMat = single(magic(3)) / 7;
logicalMat = magic(4) > 8;
logicalScalar = true;
complexMat = magic(3) + 1i * (magic(3)');
complexScalar = 1 + 2i;
complexSingle = single([1+1i, 2-2i]);
specials = [NaN Inf -Inf 0 -0 1e-300 1e300 realmin realmax eps];
emptyDouble = [];
emptyZeros03 = zeros(0, 3);
charRow = 'hello world';
charMat = ['abc'; 'def'; 'ghi'];
charUnicode = ['h' char(233) 'llo w' char(246) 'rld ' char(10003) ' ' char(20320) char(22909)];
emptyChar = '';
cellArr = {1, 'two', [3 4 5]; {6}, struct('a', 7), []};
cellEmpty = {};
cellCol = {'alpha'; 'beta'; 'gamma'};
cellNested = {{{'deep'}}};
scalarStruct = struct('name', 'test', 'value', 42, ...
    'nested', struct('deep', {{1, 2, 3}}, 'mat', eye(3), 'txt', 'inner'));
structArr = struct('id', {1, 2, 3}, 'label', {'one', 'two', 'three'});
structArr2d = repmat(struct('x', 1, 'y', 'z'), 2, 2);
emptyStruct = struct([]);
structNoFields = struct();
structArrNoFields = repmat(struct(), 1, 3);
sparseMat = sparse([1 3 5], [1 2 3], [10 20 30], 5, 4);
sparseLogical = sparse(logical(eye(3)));
sparseComplex = sparse([1 2], [1 2], [1+1i, 2-2i]);
sparseEmpty = sparse(3, 3);
nd3 = reshape(1:24, 2, 3, 4);
nd4 = reshape(1:120, 2, 3, 4, 5);
nd3single = single(reshape(1:24, 2, 3, 4));
fh = @(x) x.^2 + 1;
fhNamed = @sin;
strScalar = "a string";
strArr = ["one", "two"; "three", "four"];
strMissing = string(missing);
dt = datetime(2026, 9, 4);
dur = seconds(90);
tbl = table([1; 2; 3], {'a'; 'b'; 'c'}, 'VariableNames', {'num', 'ch'});
catg = categorical({'red', 'green', 'red'});
mapObj = containers.Map({'a', 'b'}, {1, 2});
timeTbl = timetable(seconds(1:3)', [4; 5; 6]);

basicVars = {'scalarDouble', 'rowVec', 'colVec', 'matrix', 'wideMatrix', 'tallMatrix', ...
    'int32Mat', 'uint8Mat', 'int16Neg', 'int64Vec', 'uint64Max', 'singleMat', ...
    'logicalMat', 'logicalScalar', 'complexMat', 'complexScalar', 'complexSingle', 'specials', ...
    'emptyDouble', 'emptyZeros03', 'charRow', 'charMat', 'charUnicode', 'emptyChar', ...
    'cellArr', 'cellEmpty', 'cellCol', 'cellNested', 'scalarStruct', 'structArr', ...
    'structArr2d', 'emptyStruct', 'structNoFields', 'structArrNoFields', ...
    'sparseMat', 'sparseLogical', 'sparseComplex', 'sparseEmpty', 'nd3', 'nd4', 'nd3single', ...
    'fh', 'fhNamed'};
objVars = {'strScalar', 'strArr', 'strMissing', 'dt', 'dur', 'tbl', 'catg', 'mapObj', 'timeTbl'};

save(fullfile(outdir, 'zoo_v6.mat'), basicVars{:}, '-v6');
save(fullfile(outdir, 'zoo_v7.mat'), basicVars{:}, objVars{:}, '-v7');
save(fullfile(outdir, 'zoo_v73.mat'), basicVars{:}, objVars{:}, '-v7.3');
save(fullfile(outdir, 'zoo_v73_nocomp.mat'), basicVars{:}, objVars{:}, '-v7.3', '-nocompression');
fprintf('wrote zoo fixtures to %s\n', outdir);
end
